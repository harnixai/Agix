//! agixd 替自己做的进程级的事里，嵌入方也要做的那几件（见指南 §6 的对照表）。

pub use ::assembly::discovery::identity;
pub use ::assembly::logging::log_filter;
#[cfg(feature = "otel")]
pub use ::assembly::telemetry_otel::shutdown_process_exporter;

pub mod crash {
    //! 崩溃时先杀掉命令的进程组再写崩溃记录的 panic 钩子；一个进程装一次。

    pub use ::platform::crash::install;
}

pub mod orphans {
    //! 上一个实例留下的进程组：启动时回收，运行时登记。

    pub use ::platform::orphans::{persist_to, reap_previous};
}
