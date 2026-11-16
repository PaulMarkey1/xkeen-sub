/// Canonical identity data derived from a subscription's position and optional name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubscriptionIdentity {
    pub(crate) id: String,
    pub(crate) prefix: String,
}

impl SubscriptionIdentity {
    pub(crate) fn from_number(number: usize, name: Option<&str>) -> Self {
        let id = format!("sub{number}");
        let prefix = match name {
            Some(value) => format!("{id}--{value}--"),
            None => format!("{id}--"),
        };
        Self { id, prefix }
    }
}

/// Extracts the numeric identifier from an exact subscription ID such as "sub12".
pub(crate) fn number_from_id(id: &str) -> Option<u64> {
    let number = id.strip_prefix("sub")?;
    if number.is_empty() || !number.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

/// Extracts a subscription number from a prefixed outbound tag such as "sub12--Name--node".
pub(crate) fn number_from_tag(tag: &str) -> Option<u64> {
    let rest = tag.strip_prefix("sub")?;
    let (number, suffix) = rest.split_once("--")?;
    if suffix.is_empty() {
        return None;
    }
    number_from_id(&format!("sub{number}"))
}

#[cfg(test)]
mod tests {
    use super::{number_from_id, number_from_tag, SubscriptionIdentity};

    #[test]
    fn creates_identity_with_and_without_name() {
        assert_eq!(
            SubscriptionIdentity::from_number(3, Some("Germany")),
            SubscriptionIdentity {
                id: "sub3".to_owned(),
                prefix: "sub3--Germany--".to_owned(),
            }
        );
        assert_eq!(
            SubscriptionIdentity::from_number(4, None),
            SubscriptionIdentity {
                id: "sub4".to_owned(),
                prefix: "sub4--".to_owned(),
            }
        );
    }

    #[test]
    fn parses_only_canonical_subscription_ids() {
        assert_eq!(number_from_id("sub12"), Some(12));
        assert_eq!(number_from_id("sub0"), Some(0));
        assert_eq!(number_from_id("sub"), None);
        assert_eq!(number_from_id("sub1x"), None);
        assert_eq!(number_from_id("xsub1"), None);
    }

    #[test]
    fn parses_subscription_number_from_outbound_tag() {
        assert_eq!(number_from_tag("sub12--Germany--node"), Some(12));
        assert_eq!(number_from_tag("sub12--node"), Some(12));
        assert_eq!(number_from_tag("sub12"), None);
        assert_eq!(number_from_tag("sub12--"), None);
        assert_eq!(number_from_tag("custom"), None);
    }
}
