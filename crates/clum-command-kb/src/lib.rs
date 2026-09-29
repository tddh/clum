//! clum 命令规则引擎：命令归一化 + 风险知识库 + 三态裁决。
//!
//! 本 crate 是纯逻辑（除读取配置文件外无 IO、无网络），便于单测覆盖所有绕过路径。
//! 调用方（clum-mcp）负责提供配置内容与上下文，并处理裁决结果的副作用（审批、审计）。

pub mod effect;
// 以下模块由后续 Task 逐个实现并放开（Task 1 仅建立 effect）：
// pub mod evaluate;
// pub mod kb;
// pub mod normalize;
// pub mod rules;

pub use effect::Effect;
// pub use evaluate::{evaluate, Decision, EvalContext, Verdict};
// pub use kb::KnowledgeBase;
// pub use normalize::{normalize, Hazard, NormalizeOutcome, SimpleCommand};
// pub use rules::RuleSet;
