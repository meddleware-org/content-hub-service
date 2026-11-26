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
    /// The kind of enumeration (its database type name).
    pub kind: &'static str,
    /// The text that was not recognised.
    pub value: String,
}

/// Define an enumeration stored as check-constrained text: its variants, `ALL`, `KIND`, the stored spelling and the parse and display impls.
macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $name:ident ($kind:literal) { $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name {
            /// One value of the enumeration, serialised and stored as the text on the `serde` rename.
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
            /// Write the stored spelling.
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            /// An unknown spelling is an `UnknownValue`.
            type Err = UnknownValue;
            /// Parse the stored spelling.
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
        /// Infrastructure work.
        Infra => "infra",
        /// Work on a social channel.
        Social => "social",
        /// Content to write or publish.
        Content => "content",
        /// A submission to a listing or directory.
        Listing => "listing",
        /// An advertising campaign.
        AdCampaign => "ad_campaign",
        /// Research.
        Research => "research",
        /// Engineering work.
        Engineering => "engineering",
        /// Work to make the project discoverable by bots and crawlers.
        BotDiscoverability => "bot_discoverability",
        /// Work on the trustless features.
        Trustless => "trustless",
        /// Anything else.
        Other => "other",
    }
}

text_enum! {
    /// `roadmap_items.status`.
    ItemStatus ("item status") {
        /// Not started.
        Planned => "planned",
        /// Under way.
        InProgress => "in_progress",
        /// Finished.
        Done => "done",
        /// Dropped.
        Cancelled => "cancelled",
    }
}

text_enum! {
    /// `tasks.task_type`.
    TaskType ("task type") {
        /// Set up an account on a platform.
        AccountSetup => "ACCOUNT_SETUP",
        /// Write or schedule a social post.
        SocialPost => "SOCIAL_POST",
        /// Write a blog article.
        BlogArticle => "BLOG_ARTICLE",
        /// Submit the project to a listing.
        ListingSubmission => "LISTING_SUBMISSION",
        /// Run an advertising campaign.
        AdCampaign => "AD_CAMPAIGN",
        /// Run a campaign with key opinion leaders.
        KolCampaign => "KOL_CAMPAIGN",
        /// An engineering task.
        Engineering => "ENGINEERING",
        /// A research task.
        Research => "RESEARCH",
        /// Any other task.
        Other => "OTHER",
    }
}

text_enum! {
    /// `tasks.status` (transitions: [`crate::task_state`]).
    TaskStatus ("task status") {
        /// Not started.
        Planned => "planned",
        /// Ready to start.
        Ready => "ready",
        /// Being worked on.
        InProgress => "in_progress",
        /// Finished.
        Done => "done",
        /// Waiting on something; a reason is required.
        Blocked => "blocked",
    }
}

text_enum! {
    /// `platform_definitions.category`.
    PlatformCategory ("platform category") {
        /// A social network.
        Social => "social",
        /// A forum.
        Forum => "forum",
        /// A directory or listing site.
        Directory => "directory",
        /// An advertising network.
        AdNetwork => "ad_network",
        /// An event.
        Event => "event",
        /// A content site.
        Content => "content",
        /// An on-chain destination.
        Onchain => "onchain",
        /// A partnership.
        Partnership => "partnership",
        /// A registry.
        Registry => "registry",
        /// A channel the project owns.
        OwnedChannel => "owned_channel",
        /// An internal destination.
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
        /// No markup.
        Plain => "plain",
        /// Markdown.
        Markdown => "markdown",
        /// Discord's Markdown dialect.
        DiscordMd => "discord_md",
        /// The HTML subset Telegram accepts.
        TelegramHtml => "telegram_html",
        /// BBCode.
        Bbcode => "bbcode",
    }
}

text_enum! {
    /// `posts.status`.
    PostStatus ("post status") {
        /// Being written.
        Draft => "draft",
        /// Ready to be scheduled for platforms.
        Ready => "ready",
        /// Kept for the record and no longer used.
        Archived => "archived",
    }
}

text_enum! {
    /// `platform_instances.status` (transitions: [`crate::instance_state`]).
    InstanceStatus ("instance status") {
        /// Planned for a platform and not yet queued.
        Planned => "planned",
        /// Waiting for the worker.
        Queued => "queued",
        /// Claimed by the worker; the send is in progress or its outcome is not yet recorded.
        Sending => "sending",
        /// Accepted by the platform.
        Posted => "posted",
        /// Refused by the platform: it provably did not take effect.
        Failed => "failed",
        /// It may or may not have gone out; only an operator can settle it.
        Unknown => "unknown",
        /// A person has to post it, or the worker was stopped at claim time.
        ManualRequired => "manual_required",
        /// Dropped.
        Cancelled => "cancelled",
    }
}

text_enum! {
    /// `platform_instances.external_status`: progress reported by a third party (a KOL, a directory).
    ExternalStatus ("external status") {
        /// Proposed to the third party.
        Proposed => "proposed",
        /// The third party confirmed.
        Confirmed => "confirmed",
        /// The third party published it.
        Published => "published",
        /// Submitted to the directory or platform.
        Submitted => "submitted",
        /// Under review by the third party.
        InReview => "in_review",
        /// Approved by the third party.
        Approved => "approved",
        /// Rejected by the third party.
        Rejected => "rejected",
    }
}

text_enum! {
    /// `delivery_attempts.outcome`.
    DeliveryOutcome ("delivery outcome") {
        /// The platform accepted the send.
        Success => "success",
        /// The send failed and provably did not take effect.
        Failed => "failed",
        /// The request may or may not have taken effect (for example a timeout after sending). Never retried
        /// automatically.
        Unknown => "unknown",
        /// The platform answered with a rate limit.
        RateLimited => "rate_limited",
        /// The platform refused the content itself.
        ValidationError => "validation_error",
        /// Nothing was sent: a dry run (publishing is off).
        DryRun => "dry_run",
    }
}

text_enum! {
    /// `task_links.link_kind`.
    LinkKind ("link kind") {
        /// A link to a post.
        Post => "post",
        /// A link to a platform instance.
        PlatformInstance => "platform_instance",
        /// A link to something outside the hub.
        External => "external",
    }
}

text_enum! {
    /// `spend_caps.scope`.
    SpendScope ("spend scope") {
        /// A cap on all spending.
        Total => "total",
        /// A cap on one platform.
        Platform => "platform",
        /// A cap on one target.
        Target => "target",
    }
}

text_enum! {
    /// `spend_ledger.kind`.
    SpendKind ("spend kind") {
        /// Money set aside for a send.
        Reserve => "reserve",
        /// Money recorded as spent.
        Settle => "settle",
        /// A reservation given back.
        Release => "release",
    }
}

text_enum! {
    /// `snapshots.network`.
    Network ("network") {
        /// A local test network.
        Localnet => "localnet",
        /// The public test network.
        Testnet => "testnet",
        /// The main network.
        Mainnet => "mainnet",
    }
}

/// Every enumeration with the `(table, column)` it is stored in, for the store's parity test.
pub fn stored_enumerations() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    /// The stored spellings of every value, in declaration order.
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

/// Unit tests for this module.
#[cfg(test)]
mod tests {
    use super::*;

    /// Every value round trips through its spelling and serde.
    #[test]
    fn every_value_round_trips_through_its_spelling_and_serde() {
        /// Check one enumeration: spellings are unique and round trip through parsing and serde.
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

    /// Unknown spellings are refused with their kind.
    #[test]
    fn unknown_spellings_are_refused_with_their_kind() {
        let e = "Queued".parse::<InstanceStatus>().unwrap_err();
        assert_eq!(e, UnknownValue { kind: "instance status", value: "Queued".into() });
        assert!(serde_json::from_str::<TaskType>("\"social_post\"").is_err());
    }

    /// The parity list covers every enumeration once.
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
