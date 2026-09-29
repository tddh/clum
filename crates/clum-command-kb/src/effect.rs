use serde::{Deserialize, Serialize};

/// 三态裁决动作。
///
/// `Ord` 顺序刻意设计为 `Allow < Ask < Deny`：复合命令用 `max()` 聚合时
/// 自动取最严动作，这是 fail-closed 的机制保证，不是巧合。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    #[default]
    Ask,
    Deny,
}

impl Effect {
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Allow => "allow",
            Effect::Ask => "ask",
            Effect::Deny => "deny",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_picks_strictest() {
        assert_eq!(Effect::Allow.max(Effect::Ask), Effect::Ask);
        assert_eq!(Effect::Ask.max(Effect::Deny), Effect::Deny);
        assert_eq!(Effect::Allow.max(Effect::Deny), Effect::Deny);
        assert_eq!(Effect::Allow.max(Effect::Allow), Effect::Allow);
    }

    #[test]
    fn default_is_ask_never_allow() {
        assert_eq!(Effect::default(), Effect::Ask);
    }

    #[test]
    fn iter_fold_max_is_fail_closed() {
        let segs = [Effect::Allow, Effect::Deny, Effect::Allow];
        let agg = segs.iter().copied().fold(Effect::Allow, Effect::max);
        assert_eq!(agg, Effect::Deny);
    }

    #[test]
    fn serde_roundtrip_snake_case() {
        assert_eq!(serde_json::to_string(&Effect::Ask).unwrap(), "\"ask\"");
        assert_eq!(
            serde_json::from_str::<Effect>("\"deny\"").unwrap(),
            Effect::Deny
        );
    }
}
