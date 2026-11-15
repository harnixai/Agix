//! 会话以哪个场景跑：内建场景、注册表，以及自定义场景要实现的东西。

pub use ::base::interface::scene::{
    ExecutionParams, ReminderContext, ScenePrompt, ScenePromptContext, SceneSpec, TokenBudget,
};
pub use ::scene::builtin;
pub use ::scene::SceneRegistry;

pub mod code {
    //! 内建的 code 场景。

    pub use ::scene::code::{spec, Interaction};
}

pub mod harnix {
    //! 内建的 harnix 场景。

    pub use ::scene::harnix::spec;
}

pub mod mini {
    //! 内建的 mini 场景。

    pub use ::scene::mini::spec;
}
