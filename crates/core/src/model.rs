//! The enumerations of the data model.
//!
//! Each one is stored as check-constrained text in PostgreSQL (`crates/store/migrations`). Its `ALL` list is
//! the single Rust source of the allowed values; the store's tests compare it with the database's CHECK
//! constraint, so the two cannot drift.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A value that is not one of an enumeration's allowed spellings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} value {value:?}")]
pub struct UnknownValue {
    pub kind: &'static str,
    pub value: String,
}

macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $name:ident ($kind:literal) { $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name {
            $( $(#[$vmeta])* #[serde(rename = $text)] $variant ),+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [$name] = &[ $( $name::$variant ),+ ];
            /// The name of the database column type this enumeration is stored in (for messages).
            pub const KIND: &'static str = $kind;

            /// The stored spelling.
            pub const fn as_str(self) -> &'static str {
                match self { $( $name::$variant => $text ),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = UnknownValue;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $text => Ok($name::$variant), )+
                    _ => Err(UnknownValue { kind: $kind, value: s.to_owned() }),
                }
            }
        }
    };
}

text_enum! {
    /// `roadmap_items.item_type`.
    ItemType ("item type") {
        Infra => "infra",
        Social => "social",
        Content => "content",
        Listing => "listing",
        AdCampaign => "ad_campaign",
        Research => "research",
        Engineering => "engineering",
        BotDiscoverability => "bot_discoverability",
        Trustless => "trustless",
        Other => "other",
    }
}

text_enum! {
    /// `roadmap_items.status`.
    ItemStatus ("item status") {
        Planned => "planned",
        InProgress => "in_progress",
        Done => "done",
        Cancelled => "cancelled",
    }
}

text_enum! {
    /// `tasks.task_type`.
    TaskType ("task type") {
        AccountSetup => "ACCOUNT_SETUP",
        SocialPost => "SOCIAL_POST",
        BlogArticle => "BLOG_ARTICLE",
        ListingSubmission => "LISTING_SUBMISSION",
        AdCampaign => "AD_CAMPAIGN",
        KolCampaign => "KOL_CAMPAIGN",
        Engineering => "ENGINEERING",
        Research => "RESEARCH",
        Other => "OTHER",
    }
}

text_enum! {
    /// `tasks.status` (transitions: [`crate::task_state`]).
    TaskStatus ("task status") {
        Planned => "planned",
        Ready => "ready",
        InProgress => "in_progress",
        Done => "done",
        Blocked => "blocked",
    }
}

text_enum! {
    /// `platform_definitions.category`.
    PlatformCategory ("platform category") {
        Social => "social",
        Forum => "forum",
        Directory => "directory",
        AdNetwork => "ad_network",
        Event => "event",
        Content => "content",
        Onchain => "onchain",
        Partnership => "partnership",
        Registry => "registry",
        OwnedChannel => "owned_channel",
        Internal => "internal",
    }
}

text_enum! {
    /// `platform_definitions.posting_mode`: how a post reaches the platform.
    PostingMode ("posting mode") {
        /// An authenticated API call by the worker.
        Api => "api",
        /// A webhook URL (itself a secret) called by the worker.
        Webhook => "webhook",
        /// Inclusion in the hub's own feeds (RSS / JSON Feed), served by the public process.
        Feed => "feed",
        /// A person posts it (with a prefilled intent link where the platform has one) and records the result.
        Manual => "manual",
        /// Not a posting destination (tracking only).
        None => "none",
    }
}

text_enum! {
    /// `platform_definitions.counting_method`: how a platform counts towards its limit.
    CountingMethod ("counting method") {
        /// twitter-text v3 weighting: every URL counts 23, text NFC-normalised, wide ranges weigh 2.
        XWeighted => "x_weighted",
        /// Extended grapheme clusters.
        Graphemes => "graphemes",
        /// UTF-16 code units.
        Utf16 => "utf16",
        /// Unicode scalar values.
        Codepoints => "codepoints",
        /// UTF-8 bytes.
        Bytes => "bytes",
    }
}

text_enum! {
    /// `platform_definitions.markup`: what a renderer produces for the platform.
    Markup ("markup") {
        Plain => "plain",
        Markdown => "markdown",
        DiscordMd => "discord_md",
        TelegramHtml => "telegram_html",
        Bbcode => "bbcode",
    }
}

text_enum! {
    /// `posts.status`.
    PostStatus ("post status") {
        Draft => "draft",
        Ready => "ready",
        Archived => "archived",
    }
}

text_enum! {
    /// `platform_instances.status` (transitions: [`crate::instance_state`]).
    InstanceStatus ("instance status") {
        Planned => "planned",
        Queued => "queued",
        Sending => "sending",
        Posted => "posted",
        Failed => "failed",
        Unknown => "unknown",
        ManualRequired => "manual_required",
        Cancelled => "cancelled",
    }
}

text_enum! {
    /// `platform_instances.external_status`: progress reported by a third party (a KOL, a directory).
    ExternalStatus ("external status") {
        Proposed => "proposed",
        Confirmed => "confirmed",
        Published => "published",
        Submitted => "submitted",
        InReview => "in_review",
        Approved => "approved",
        Rejected => "rejected",
    }
}

text_enum! {
    /// `delivery_attempts.outcome`.
    DeliveryOutcome ("delivery outcome") {
        Success => "success",
        Failed => "failed",
        /// The request may or may not have taken effect (for example a timeout after sending). Never retried
        /// automatically.
        Unknown => "unknown",
        RateLimited => "rate_limited",
        ValidationError => "validation_error",
        DryRun => "dry_run",
    }
}

text_enum! {
    /// `task_links.link_kind`.
    LinkKind ("link kind") {
        Post => "post",
        PlatformInstance => "platform_instance",
        External => "external",
    }
}

text_enum! {
    /// `spend_caps.scope`.
    SpendScope ("spend scope") {
        Total => "total",
        Platform => "platform",
        Target => "target",
    }
}

text_enum! {
    /// `spend_ledger.kind`.
    SpendKind ("spend kind") {
        Reserve => "reserve",
        Settle => "settle",
        Release => "release",
    }
}

text_enum! {
    /// `snapshots.network`.
    Network ("network") {
        Localnet => "localnet",
        Testnet => "testnet",
        Mainnet => "mainnet",
    }
}

/// Every enumeration with the `(table, column)` it is stored in, for the store's parity test.
pub fn stored_enumerations() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    fn values<T: Copy>(all: &[T], f: fn(T) -> &'static str) -> Vec<&'static str> {
        all.iter().map(|v| f(*v)).collect()
    }
    vec![
        ("roadmap_items", "item_type", values(ItemType::ALL, ItemType::as_str)),
        ("roadmap_items", "status", values(ItemStatus::ALL, ItemStatus::as_str)),
        ("tasks", "task_type", values(TaskType::ALL, TaskType::as_str)),
        ("tasks", "status", values(TaskStatus::ALL, TaskStatus::as_str)),
        ("platform_definitions", "category", values(PlatformCategory::ALL, PlatformCategory::as_str)),
        ("platform_definitions", "posting_mode", values(PostingMode::ALL, PostingMode::as_str)),
        ("platform_definitions", "counting_method", values(CountingMethod::ALL, CountingMethod::as_str)),
        ("platform_definitions", "markup", values(Markup::ALL, Markup::as_str)),
        ("posts", "status", values(PostStatus::ALL, PostStatus::as_str)),
        ("platform_instances", "status", values(InstanceStatus::ALL, InstanceStatus::as_str)),
        ("platform_instances", "external_status", values(ExternalStatus::ALL, ExternalStatus::as_str)),
        ("delivery_attempts", "outcome", values(DeliveryOutcome::ALL, DeliveryOutcome::as_str)),
        ("task_links", "link_kind", values(LinkKind::ALL, LinkKind::as_str)),
        ("spend_caps", "scope", values(SpendScope::ALL, SpendScope::as_str)),
        ("spend_ledger", "kind", values(SpendKind::ALL, SpendKind::as_str)),
        ("snapshots", "network", values(Network::ALL, Network::as_str)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_value_round_trips_through_its_spelling_and_serde() {
        fn check<T>(all: &[T], as_str: fn(T) -> &'static str)
        where
            T: Copy + PartialEq + fmt::Debug + FromStr<Err = UnknownValue> + Serialize + for<'de> Deserialize<'de>,
        {
            let mut seen = std::collections::BTreeSet::new();
            for v in all {
                let s = as_str(*v);
                assert!(seen.insert(s), "duplicate spelling {s}");
                assert_eq!(s.parse::<T>().as_ref(), Ok(v));
                assert_eq!(serde_json::to_string(v).expect("serialise"), format!("\"{s}\""));
                assert_eq!(serde_json::from_str::<T>(&format!("\"{s}\"")).expect("deserialise"), *v);
            }
        }
        check(ItemType::ALL, ItemType::as_str);
        check(ItemStatus::ALL, ItemStatus::as_str);
        check(TaskType::ALL, TaskType::as_str);
        check(TaskStatus::ALL, TaskStatus::as_str);
        check(PlatformCategory::ALL, PlatformCategory::as_str);
        check(PostingMode::ALL, PostingMode::as_str);
        check(CountingMethod::ALL, CountingMethod::as_str);
        check(Markup::ALL, Markup::as_str);
        check(PostStatus::ALL, PostStatus::as_str);
        check(InstanceStatus::ALL, InstanceStatus::as_str);
        check(ExternalStatus::ALL, ExternalStatus::as_str);
        check(DeliveryOutcome::ALL, DeliveryOutcome::as_str);
        check(LinkKind::ALL, LinkKind::as_str);
        check(SpendScope::ALL, SpendScope::as_str);
        check(SpendKind::ALL, SpendKind::as_str);
        check(Network::ALL, Network::as_str);
    }

    #[test]
    fn unknown_spellings_are_refused_with_their_kind() {
        let e = "Queued".parse::<InstanceStatus>().unwrap_err();
        assert_eq!(e, UnknownValue { kind: "instance status", value: "Queued".into() });
        assert!(serde_json::from_str::<TaskType>("\"social_post\"").is_err());
    }

    #[test]
    fn the_parity_list_covers_every_enumeration_once() {
        let list = stored_enumerations();
        let mut columns: Vec<_> = list.iter().map(|(t, c, _)| (*t, *c)).collect();
        columns.sort();
        columns.dedup();
        assert_eq!(columns.len(), list.len());
        assert_eq!(list.len(), 16);
    }
}
