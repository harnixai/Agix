//! 宿主要实现的 trait 与它们的数据类型，按扩展点分子模块；每个点的触发时机与信任级别见 `docs/extension_points.md`。

    pub mod exec {
        //! 工具与引擎碰机器的四个执行层契约；`ExecProviders::in_process()` 是不碰机器的一套。

        pub use ::base::interface::exec::ExecProviders;
        pub use ::base::interface::exec::Process;
        pub use ::base::interface::exec::FileSystem;
        pub use ::base::interface::exec::Network;
        pub use ::base::interface::exec::Sandbox;
    }

    pub mod model {
        //! 模型从哪来：Anthropic 线协议的客户端，或整个换掉 `Model`。

        pub use ::model::client::AnthropicClient;
        pub use ::model::client::AuthMode;
        pub use ::model::client::HttpAnthropicClient;
        pub use ::base::interface::model::Model;
        pub use ::base::interface::model::ModelEvent;
        pub use ::base::interface::model::ToolDef;
    }

    pub mod credentials {
        //! 配置没写死的密钥从哪来（`Assembly::credentials`）。

        pub use ::base::interface::credentials::CredentialSource;
        pub use ::base::interface::credentials::Secret;
    }

    pub mod prompt {
        //! 系统提示：块、组装器、提示中间件。

        pub use ::base::prompt::PromptBlock;
        pub use ::base::prompt::BlockRole;
        pub use ::base::prompt::CacheStrategy;
        pub use ::base::prompt::BlockOrigin;
        pub use ::base::interface::prompt_assembler::PromptAssembler;
        pub use ::base::interface::prompt_assembler::AssemblyRequest;
        pub use ::base::interface::prompt_middleware::PromptMiddleware;
        pub use ::base::interface::prompt_middleware::PromptCall;
        pub use ::base::interface::prompt_middleware::PromptDecision;
        pub use ::base::interface::prompt_middleware::PromptDelivery;
        pub use ::base::interface::prompt_middleware::PromptAttachment;
        pub use ::base::interface::prompt_middleware::PromptAttachmentKind;
    }

    pub mod rules {
        //! 规则文档从哪来（`Builder::rule_provider`）。

        pub use ::base::interface::instruction_provider::RuleProvider;
        pub use ::base::rules::RuleEntry;
    }

    pub mod tools {
        //! 宿主用 Rust 写的工具与围住工具调用的中间件。

        pub use ::base::tool::Tool;
        pub use ::base::tool::ToolSpec;
        pub use ::base::tool::CallTraits;
        pub use ::base::tool::ToolContext;
        pub use ::base::tool::ToolResult;
        pub use ::base::tool::ToolResultContent;
        pub use ::base::tool::ProgressSender;
        pub use ::base::tool::ProgressCallback;
        pub use ::base::tool::PromptContext;
        pub use ::base::tool::PathStyle;
        pub use ::base::error::ToolError;
        pub use ::base::interface::permission::PermissionFacts;
        pub use ::base::interface::tool_middleware::ToolMiddleware;
        pub use ::base::interface::tool_middleware::ToolCall;
        pub use ::base::interface::tool_middleware::ToolExec;
        pub use ::base::interface::tool_middleware::ToolAnswer;
        pub use ::base::interface::tool_middleware::ToolOutcome;
        pub use ::base::interface::tool_middleware::NextDispatch;
        pub use ::base::interface::tool_middleware::dispatch_through;
    }

    pub mod session_state {
        //! 工具经 `ToolContext` 看到的会话状态契约（引擎只有一个实现，宿主通常只读）。

        pub use ::base::context::Session;
    }
