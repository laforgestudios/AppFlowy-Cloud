use app_error::AppError;
use serde::{Deserialize, Serialize};

/// Where a created view goes among the children of its Folder parent
/// (web `CreationViewPosition`). An absent position means `end`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CreationViewPosition {
  Start,
  #[default]
  End,
  /// Right after this sibling, which must still be a child of the parent.
  After {
    view_id: String,
  },
  /// Index among the children, clamped to the end. The web derives it from the visible
  /// children, which are never more than the Folder children.
  Index {
    index: u32,
  },
}

/// Resolves `position` against the Folder children of the parent (in Folder order, trashed and
/// private views included) into the index for `views.insert` (`None` appends).
/// Must run under the workspace lock so that the children cannot change before insertion.
pub fn resolve_insert_index(
  siblings: &[String],
  position: &CreationViewPosition,
) -> Result<Option<u32>, AppError> {
  match position {
    CreationViewPosition::Start => Ok(Some(0)),
    CreationViewPosition::End => Ok(None),
    CreationViewPosition::After { view_id } => siblings
      .iter()
      .position(|sibling| sibling == view_id)
      .map(|index| Some(index as u32 + 1))
      .ok_or_else(|| {
        AppError::InvalidRequest(format!(
          "the insertion sibling {} is no longer a child of the parent view",
          view_id
        ))
      }),
    CreationViewPosition::Index { index } => Ok(Some((*index).min(siblings.len() as u32))),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn siblings() -> Vec<String> {
    vec!["a".to_string(), "b".to_string(), "c".to_string()]
  }

  #[test]
  fn parses_web_positions() {
    let parse = |json: &str| serde_json::from_str::<CreationViewPosition>(json).unwrap();
    assert_eq!(parse(r#"{"kind":"start"}"#), CreationViewPosition::Start);
    assert_eq!(parse(r#"{"kind":"end"}"#), CreationViewPosition::End);
    assert_eq!(
      parse(r#"{"kind":"after","view_id":"b"}"#),
      CreationViewPosition::After {
        view_id: "b".to_string()
      }
    );
    assert_eq!(
      parse(r#"{"kind":"index","index":2}"#),
      CreationViewPosition::Index { index: 2 }
    );
    assert!(
      serde_json::from_str::<CreationViewPosition>(r#"{"kind":"index","index":-1}"#).is_err()
    );
    assert!(serde_json::from_str::<CreationViewPosition>(r#"{"kind":"before"}"#).is_err());
    assert!(serde_json::from_str::<CreationViewPosition>(r#"{"kind":"after"}"#).is_err());
  }

  #[test]
  fn start_and_end() {
    assert_eq!(
      resolve_insert_index(&siblings(), &CreationViewPosition::Start).unwrap(),
      Some(0)
    );
    assert_eq!(
      resolve_insert_index(&siblings(), &CreationViewPosition::End).unwrap(),
      None
    );
    assert_eq!(
      resolve_insert_index(&[], &CreationViewPosition::Start).unwrap(),
      Some(0)
    );
  }

  #[test]
  fn after_sibling() {
    let after = |id: &str| CreationViewPosition::After {
      view_id: id.to_string(),
    };
    assert_eq!(
      resolve_insert_index(&siblings(), &after("a")).unwrap(),
      Some(1)
    );
    assert_eq!(
      resolve_insert_index(&siblings(), &after("c")).unwrap(),
      Some(3)
    );
  }

  #[test]
  fn after_a_sibling_that_moved_out_is_rejected() {
    let err = resolve_insert_index(
      &siblings(),
      &CreationViewPosition::After {
        view_id: "z".to_string(),
      },
    )
    .unwrap_err();
    assert!(matches!(err, AppError::InvalidRequest(_)));
  }

  #[test]
  fn index_is_clamped() {
    let index = |index: u32| CreationViewPosition::Index { index };
    assert_eq!(
      resolve_insert_index(&siblings(), &index(0)).unwrap(),
      Some(0)
    );
    assert_eq!(
      resolve_insert_index(&siblings(), &index(2)).unwrap(),
      Some(2)
    );
    assert_eq!(
      resolve_insert_index(&siblings(), &index(3)).unwrap(),
      Some(3)
    );
    assert_eq!(
      resolve_insert_index(&siblings(), &index(42)).unwrap(),
      Some(3)
    );
  }

  #[test]
  fn absent_position_defaults_to_end() {
    assert_eq!(CreationViewPosition::default(), CreationViewPosition::End);
  }
}
