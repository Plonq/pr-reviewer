//! The viewer's submitted review is the baseline, never local last-seen state.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewBaseline {
    pub commit: String,
    pub submitted_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewChanges {
    Available(ReviewBaseline),
    Unavailable(String),
}

impl Default for ReviewChanges {
    fn default() -> Self {
        Self::Unavailable("Review history unavailable. Refresh to try again.".into())
    }
}

impl ReviewChanges {
    pub fn baseline(&self) -> Option<&ReviewBaseline> {
        match self {
            Self::Available(review) => Some(review),
            Self::Unavailable(_) => None,
        }
    }
}

pub(crate) fn valid_oid(oid: &str) -> bool {
    oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

/// REST pages may not be ordered by submission time: a draft can be created
/// before another review, then submitted after it. Dismissed reviews still mark
/// code that was reviewed; pending reviews and issue comments do not.
pub(crate) fn latest_review(
    pages: serde_json::Value,
    viewer: &str,
) -> Result<Option<ReviewBaseline>, crate::ClientError> {
    #[derive(serde::Deserialize)]
    struct Review {
        user: Option<crate::Author>,
        state: String,
        submitted_at: Option<String>,
        commit_id: Option<String>,
        id: u64,
    }
    let pages: Vec<Vec<Review>> = serde_json::from_value(pages)
        .map_err(|e| crate::ClientError::Failed(format!("invalid review history: {e}")))?;
    let latest = pages
        .into_iter()
        .flatten()
        .filter(|review| {
            review
                .user
                .as_ref()
                .is_some_and(|user| user.login.eq_ignore_ascii_case(viewer))
                && matches!(
                    review.state.as_str(),
                    "APPROVED" | "CHANGES_REQUESTED" | "COMMENTED" | "DISMISSED"
                )
                && review
                    .submitted_at
                    .as_ref()
                    .is_some_and(|date| !date.is_empty())
        })
        .max_by(|a, b| (&a.submitted_at, a.id).cmp(&(&b.submitted_at, b.id)));
    latest
        .map(|review| {
            let commit = review
                .commit_id
                .filter(|oid| valid_oid(oid))
                .ok_or_else(|| {
                    crate::ClientError::Failed("the latest review's commit is unavailable".into())
                })?;
            Ok(ReviewBaseline {
                commit,
                submitted_at: review.submitted_at.unwrap(),
            })
        })
        .transpose()
}

pub(crate) fn validate_comparison(
    review: ReviewBaseline,
    value: &serde_json::Value,
) -> Result<ReviewChanges, crate::ClientError> {
    let status = value["status"]
        .as_str()
        .ok_or_else(|| crate::ClientError::Failed("missing comparison status".into()))?;
    if !matches!(status, "ahead" | "identical" | "behind" | "diverged") {
        return Err(crate::ClientError::Failed(
            "unknown comparison status".into(),
        ));
    }
    let merge_base = value["merge_base_commit"]["sha"]
        .as_str()
        .ok_or_else(|| crate::ClientError::Failed("missing comparison merge base".into()))?;
    if !matches!(status, "ahead" | "identical") || merge_base != review.commit {
        return Ok(ReviewChanges::Unavailable(
            "Unavailable: this PR's history was rewritten since your last review, for example by a rebase or force-push. View all changes instead.".into(),
        ));
    }
    // Only ancestry comes from this JSON response. Its `files` listing is
    // capped at 300, independently of the raw diff fetched by comparison_patch.
    // It can include upstream files merged since review even for a small PR;
    // neither its length nor its presence determines review availability.
    Ok(ReviewChanges::Available(review))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn review(id: u64, author: &str, state: &str, date: &str, sha: &str) -> serde_json::Value {
        json!({"id":id,"user":{"login":author},"state":state,"submitted_at":date,"commit_id":sha})
    }

    #[test]
    fn latest_submission_across_pages_ignores_pending_and_other_viewers() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let pages = json!([
            [review(1, "ME", "COMMENTED", "2026-03-03T00:00:00Z", &a)],
            [
                review(2, "me", "APPROVED", "2026-03-02T00:00:00Z", &b),
                review(3, "other", "APPROVED", "2026-03-04T00:00:00Z", &b),
                review(4, "me", "PENDING", "2026-03-05T00:00:00Z", &b)
            ]
        ]);
        assert_eq!(latest_review(pages, "me").unwrap().unwrap().commit, a);
        assert!(latest_review(json!([[]]), "me").unwrap().is_none());
        assert!(latest_review(
            json!([[review(1, "me", "DISMISSED", "2026-03-03T00:00:00Z", &a)]]),
            "me"
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn missing_latest_commit_does_not_fall_back_to_an_older_review() {
        let a = "a".repeat(40);
        assert!(latest_review(
            json!([[
                review(1, "me", "APPROVED", "2026-03-02T00:00:00Z", &a),
                review(2, "me", "COMMENTED", "2026-03-03T00:00:00Z", "")
            ]]),
            "me"
        )
        .is_err());
    }

    #[test]
    fn ancestry_not_timestamps_or_commit_list_position_controls_availability() {
        let a = "a".repeat(40);
        let baseline = ReviewBaseline {
            commit: a.clone(),
            submitted_at: String::new(),
        };
        for status in ["ahead", "identical"] {
            assert!(matches!(
                validate_comparison(
                    baseline.clone(),
                    &json!({"status":status,"merge_base_commit":{"sha":a},"files":[]})
                )
                .unwrap(),
                ReviewChanges::Available(_)
            ));
        }
        for (status, sha) in [
            ("diverged", "b".repeat(40)),
            ("behind", a.clone()),
            ("ahead", "b".repeat(40)),
        ] {
            assert!(matches!(
                validate_comparison(
                    baseline.clone(),
                    &json!({"status":status,"merge_base_commit":{"sha":sha},"files":[]})
                )
                .unwrap(),
                ReviewChanges::Unavailable(_)
            ));
        }
        assert!(validate_comparison(baseline.clone(), &json!({})).is_err());
    }

    #[test]
    fn capped_or_missing_file_listing_does_not_disable_a_valid_review_baseline() {
        let a = "a".repeat(40);
        let baseline = ReviewBaseline {
            commit: a.clone(),
            submitted_at: String::new(),
        };
        for count in [0, 26, 299, 300] {
            let comparison = json!({"status":"ahead","merge_base_commit":{"sha":a},"files":vec![json!({});count]});
            assert_eq!(
                validate_comparison(baseline.clone(), &comparison).unwrap(),
                ReviewChanges::Available(baseline.clone())
            );
        }
        assert_eq!(
            validate_comparison(
                baseline.clone(),
                &json!({"status":"ahead","merge_base_commit":{"sha":a}})
            )
            .unwrap(),
            ReviewChanges::Available(baseline)
        );
    }
}
