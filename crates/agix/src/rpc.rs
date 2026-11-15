//! 同一个池的 JSON-RPC 面：要和 agixd 的客户端说同一种话时，在进程内分派或自己挂传输。

pub use ::assembly::rpc::codes;
pub use ::assembly::rpc::send_frame;
pub use ::assembly::rpc::Client;
pub use ::assembly::rpc::FrameSink;
pub use ::assembly::rpc::RpcRequest;
pub use ::assembly::rpc::RpcResponse;
pub use ::assembly::rpc::Sink;
pub use ::assembly::DaemonServer;
