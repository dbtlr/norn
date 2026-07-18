//! The tri-state presence type for the absent-vs-null frontmatter distinction.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A wire value that distinguishes three states a frontmatter-bearing field can
/// take: wire-ABSENT, wire-NULL, and a concrete value.
///
/// # NRN-222: absent frontmatter is not empty frontmatter
///
/// A Markdown document can carry no frontmatter block at all, or an empty
/// `---`/`---` block. These are different facts about the source and the wire
/// must preserve the difference — a routed result re-renders from the wire
/// value, so key presence has to carry the distinction rather than collapsing
/// both to JSON `null`:
///
/// - [`Presence::Absent`] — the source has no frontmatter block. The field is
///   OMITTED from the wire object entirely (no key).
/// - [`Presence::Null`] — the source has an empty `---`/`---` block. The field
///   is present with an explicit JSON `null`.
/// - [`Presence::Value`] — the source carries frontmatter (or any concrete
///   value); the inner value is serialized in place.
///
/// The old donor tree encoded this imperatively (`strip_absent_frontmatter`
/// removed the `frontmatter` key when the source block was absent, leaving
/// `Some(Value::Null)` to render as `"frontmatter": null`). This type carries
/// the same distinction by construction, so later `Report` types cannot lose it.
///
/// # Serde mechanics
///
/// Omitting an absent value cannot be expressed by the [`Serialize`] impl alone
/// (a serializer has no way to skip the field it was handed); the field MUST be
/// annotated `#[serde(default, skip_serializing_if = "Presence::is_absent")]` on
/// the containing struct. With that pairing:
///
/// - serialize: `Absent` is skipped by the field attribute; `Null` emits JSON
///   `null`; `Value(v)` emits `v`.
/// - deserialize: an absent key falls back to `default()` = `Absent`; an
///   explicit `null` becomes `Null`; any other value becomes `Value(v)`. This is
///   the named equivalent of `Option<Option<T>>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Presence<T> {
    /// The source has no frontmatter block — omit the field from the wire.
    Absent,
    /// The source has an empty block — send an explicit `null`.
    Null,
    /// A concrete value.
    Value(T),
}

impl<T> Default for Presence<T> {
    /// The wire default is [`Presence::Absent`] — an unmentioned key means the
    /// source carried no block, which is what `#[serde(default)]` restores.
    fn default() -> Self {
        Presence::Absent
    }
}

impl<T> Presence<T> {
    /// Whether this is [`Presence::Absent`]. This is the `skip_serializing_if`
    /// predicate that keeps an absent value off the wire entirely.
    pub fn is_absent(&self) -> bool {
        matches!(self, Presence::Absent)
    }

    /// Whether this is [`Presence::Null`] (an explicit empty block).
    pub fn is_null(&self) -> bool {
        matches!(self, Presence::Null)
    }

    /// The inner value, if this is [`Presence::Value`].
    pub fn value(&self) -> Option<&T> {
        match self {
            Presence::Value(v) => Some(v),
            _ => None,
        }
    }
}

impl<T: Serialize> Serialize for Presence<T> {
    /// `Value(v)` serializes `v`; both `Null` and `Absent` serialize as JSON
    /// `null`. `Absent` is meant to be skipped by the field's
    /// `skip_serializing_if = "Presence::is_absent"` — if it reaches here it is
    /// indistinguishable from `Null`, which is why the field attribute is
    /// mandatory (documented on the type).
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Presence::Value(value) => value.serialize(serializer),
            Presence::Null | Presence::Absent => serializer.serialize_none(),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Presence<T> {
    /// A present `null` becomes [`Presence::Null`]; any other value becomes
    /// [`Presence::Value`]. An absent key never reaches here — the field's
    /// `#[serde(default)]` supplies [`Presence::Absent`] instead.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<T>::deserialize(deserializer).map(|opt| match opt {
            Some(value) => Presence::Value(value),
            None => Presence::Null,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Rec {
        #[serde(default, skip_serializing_if = "Presence::is_absent")]
        frontmatter: Presence<serde_json::Value>,
    }

    #[test]
    fn default_is_absent() {
        assert!(Presence::<i32>::default().is_absent());
    }

    #[test]
    fn absent_omits_the_key() {
        let rec = Rec {
            frontmatter: Presence::Absent,
        };
        assert_eq!(serde_json::to_value(&rec).unwrap(), json!({}));
    }

    #[test]
    fn null_sends_explicit_null() {
        let rec = Rec {
            frontmatter: Presence::Null,
        };
        assert_eq!(
            serde_json::to_value(&rec).unwrap(),
            json!({ "frontmatter": null })
        );
    }

    #[test]
    fn value_sends_the_value() {
        let rec = Rec {
            frontmatter: Presence::Value(json!({ "status": "backlog" })),
        };
        assert_eq!(
            serde_json::to_value(&rec).unwrap(),
            json!({ "frontmatter": { "status": "backlog" } })
        );
    }

    #[test]
    fn deserialize_distinguishes_all_three_states() {
        let absent: Rec = serde_json::from_value(json!({})).unwrap();
        assert_eq!(absent.frontmatter, Presence::Absent);

        let null: Rec = serde_json::from_value(json!({ "frontmatter": null })).unwrap();
        assert_eq!(null.frontmatter, Presence::Null);

        let value: Rec = serde_json::from_value(json!({ "frontmatter": { "a": 1 } })).unwrap();
        assert_eq!(value.frontmatter, Presence::Value(json!({ "a": 1 })));
    }

    #[test]
    fn round_trips_through_all_three_states() {
        for original in [
            Rec {
                frontmatter: Presence::Absent,
            },
            Rec {
                frontmatter: Presence::Null,
            },
            Rec {
                frontmatter: Presence::Value(json!([1, 2, 3])),
            },
        ] {
            let wire = serde_json::to_value(&original).unwrap();
            let back: Rec = serde_json::from_value(wire).unwrap();
            assert_eq!(back, original);
        }
    }

    #[test]
    fn accessors() {
        assert_eq!(Presence::Value(7).value(), Some(&7));
        assert_eq!(Presence::<i32>::Null.value(), None);
        assert!(Presence::<i32>::Null.is_null());
        assert!(!Presence::Value(1).is_null());
    }
}
