//! Agent2API 网关本体（独立 crate）。
//!
//! ── 本 crate 服务两种形态 ────────────────────────────────────
//!   · Tauri 桌面端：壳进程把本 crate 当库链接，网关作为进程内 HTTP
//!     服务器跑在 tauri 的 Tokio 运行时上（`server::start`）；
//!   · headless 服务器 / Docker：`agent2api-server` 二进制（`src/bin/`）
//!     自建 Tokio 运行时、自己托管管理界面（`server::static_files`），
//!     不链接任何 GUI 依赖。
//!
//! 模块结构是**刻意保留的一层壳**：`server/`、`port_conflict.rs` 的目录
//! 与文件名与拆分前（桌面 crate 的 `src/server`）完全一致，内部所有
//! `crate::server::…` / `crate::port_conflict::…` 路径零改动 —— 桌面侧
//! 用 `use agent2api_server as server` 重导出后，两边的既有路径都继续成立。
//!
//! ── 为什么 spawn 必须经由 [`spawn_task`] ─────────────────────
//! 拆分前模块里用的是 `tauri::async_runtime::spawn`（壳提供全局运行时，
//! 任意线程可调）；本 crate 不依赖 tauri，headless 又自带 `#[tokio::main]`
//! 运行时。统一入口 [`spawn_task`] 兼容两种宿主：
//!   · 已在 Tokio 运行时上下文里（桌面：任务跑在 tauri 运行时的工作
//!     线程上；headless：`#[tokio::main]`）→ 直接 `Handle::spawn`，
//!     任务落在**宿主**的运行时上，与拆分前行为一致；
//!   · 不在运行时上下文（从任意同步线程起后台任务）→ 落到惰性创建的
//!     全局运行时，**不会 panic** —— release 档是 panic=abort，
//!     一次误用就会带走整个进程，这正是拆分前统一走
//!     `tauri::async_runtime::spawn` 的原因，本入口继承同一保证。

pub mod paths;
pub mod port_conflict;
pub mod server;
pub mod web_shim;

#[cfg(test)]
mod test_support;

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Handle, Runtime};

/// 惰性全局运行时：只承接「在非 Tokio 上下文里发起的后台任务」，
/// 正常路径（handler / 既有任务体内）不会碰到它。
static GLOBAL_RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// 项目统一的 spawn 入口：桌面与 headless 共用（原
/// `tauri::async_runtime::spawn` 的替身，见 crate 文档头）。
pub fn spawn_task<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match Handle::try_current() {
        Ok(handle) => handle.spawn(future),
        Err(_) => global_runtime().spawn(future),
    }
}

/// 网关所有 Tokio worker 线程的栈大小。
///
/// Tokio worker 线程默认栈 2 MiB。网关的请求处理链（axum handler → 转发管线 →
/// 协议转换 / 脱敏 / 记账）会为一次请求实例化很大的 async Future 状态机（观测到引用
/// ≥15 KiB 状态体的 8 KiB poll 函数），2 MiB 默认栈在长上下文大流式请求下已被观测到
/// 栈溢出硬崩（Windows Event ID 1000，异常码 0xc00000fd；详见 PR #583）。
/// 提到 16 MiB：只增加虚拟地址空间预留，物理页按实际使用提交。
///
/// 桌面（Tauri）、headless、Windows 服务、后台全局运行时**共用这一个常量**：
/// 任何一条路径漏掉都会让那条路径上的请求仍跑在 2 MiB 栈上。
pub const WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

/// 按网关约定的栈容量构造多线程运行时（所有入口共用，见 [`WORKER_STACK_BYTES`]）。
pub fn build_runtime() -> std::io::Result<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_BYTES)
        .build()
}

fn global_runtime() -> &'static Runtime {
    GLOBAL_RUNTIME.get_or_init(|| build_runtime().expect("创建后台 Tokio 运行时失败"))
}

#[cfg(test)]
mod runtime_tests {
    use super::{build_runtime, WORKER_STACK_BYTES};

    /// 在栈上放 `bytes` 字节并强制编译器保留它（不内联、不优化掉）。
    #[inline(never)]
    fn touch_stack(bytes: usize) -> u8 {
        assert!(bytes == 4 * 1024 * 1024);
        let mut buffer = [0u8; 4 * 1024 * 1024];
        // 每页写一次，保证这些页真的被提交，而不是只在栈指针上做减法。
        for offset in (0..buffer.len()).step_by(4096) {
            buffer[offset] = (offset >> 12) as u8;
        }
        std::hint::black_box(&buffer);
        buffer[buffer.len() - 1]
    }

    /// 网关的 worker 栈必须容得下 4 MiB 的栈帧：Tokio 默认的 2 MiB 会在这里栈溢出直接中止进程，
    /// 所以这条用例一旦失败就是整个测试进程崩溃，而不是一个普通断言 —— 那正是它要钉住的事故形态。
    #[test]
    fn gateway_workers_have_room_for_a_four_mib_frame() {
        assert!(WORKER_STACK_BYTES >= 8 * 1024 * 1024, "常量被调小了：{WORKER_STACK_BYTES}");
        let runtime = build_runtime().expect("构造运行时");
        let value = runtime.block_on(async {
            // `spawn` 才会落在 worker 线程上（block_on 跑在调用线程，不受 worker 栈设置影响）。
            tokio::spawn(async { touch_stack(4 * 1024 * 1024) }).await.expect("任务不应 panic")
        });
        assert_eq!(value, 0);
    }
}
