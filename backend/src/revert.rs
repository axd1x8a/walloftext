use std::collections::HashMap;

use walloftext_shared::{WorldCoords, WorldRect};

use crate::state::{Action, CellChange, StoredChunk, StoredChunkCell};

pub const MAX_REVERT_POSITIONS: usize = 250_000;

#[derive(Debug, Clone, Default)]
pub struct RevertFilter {
    pub author_id: Option<u32>,
    pub region: Option<WorldRect>,
    pub ts_range: Option<(i64, i64)>,
    pub action_id_range: Option<(u64, u64)>,
}

impl RevertFilter {
    pub fn is_unbounded(&self) -> bool {
        self.author_id.is_none()
            && self.region.is_none()
            && self.ts_range.is_none()
            && self.action_id_range.is_none()
    }

    fn action_in_scope(&self, action: &Action) -> bool {
        if let Some((lo, hi)) = self.ts_range
            && !(lo..=hi).contains(&action.ts)
        {
            return false;
        }
        if let Some((lo, hi)) = self.action_id_range
            && !(lo..=hi).contains(&action.action_id)
        {
            return false;
        }
        true
    }

    pub fn change_is_targeted(&self, action: &Action, change: &CellChange) -> bool {
        if !self.action_in_scope(action) {
            return false;
        }
        if let Some(aid) = self.author_id
            && action.author_id != aid
        {
            return false;
        }
        if let Some(rect) = self.region
            && !rect.contains(change.pos)
        {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone)]
pub enum SkipReason {
    OverwrittenByOther { by_author: u32, at_action_id: u64 },
    NoChange,
}

#[derive(Debug, Clone)]
pub enum RevertOutcome {
    Restore {
        pos: WorldCoords,
        restore_to: Option<StoredChunkCell>,
        current: Option<StoredChunkCell>,
        earliest_targeted_action_id: u64,
        targeted_touch_count: u32,
    },
    Skipped {
        pos: WorldCoords,
        reason: SkipReason,
    },
}

fn cell_eq(a: &Option<StoredChunkCell>, b: &Option<StoredChunkCell>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.ch == y.ch && x.color == y.color,
        _ => false,
    }
}

struct PosState {
    earliest_targeted_old: Option<StoredChunkCell>,
    earliest_targeted_action_id: u64,
    targeted_touch_count: u32,
    last_touch_was_targeted: bool,
    last_touch_author: u32,
    last_touch_action_id: u64,
}

pub fn compute_revert(
    filter: &RevertFilter,
    actions_oldest_first: &[Action],
    current_value: impl Fn(WorldCoords) -> Option<StoredChunkCell>,
) -> Vec<RevertOutcome> {
    let mut positions: HashMap<WorldCoords, PosState> = HashMap::new();

    for action in actions_oldest_first {
        for change in &action.changes {
            let targeted = filter.change_is_targeted(action, change);
            let entry = positions.entry(change.pos).or_insert(PosState {
                earliest_targeted_old: None,
                earliest_targeted_action_id: 0,
                targeted_touch_count: 0,
                last_touch_was_targeted: false,
                last_touch_author: action.author_id,
                last_touch_action_id: action.action_id,
            });

            if targeted {
                if entry.targeted_touch_count == 0 {
                    entry.earliest_targeted_old = change.old.clone();
                    entry.earliest_targeted_action_id = action.action_id;
                }
                entry.targeted_touch_count += 1;
            }
            entry.last_touch_was_targeted = targeted;
            entry.last_touch_author = action.author_id;
            entry.last_touch_action_id = action.action_id;
        }
    }

    positions
        .into_iter()
        .filter_map(|(pos, st)| {
            if st.targeted_touch_count == 0 {
                return None;
            }
            if !st.last_touch_was_targeted {
                return Some(RevertOutcome::Skipped {
                    pos,
                    reason: SkipReason::OverwrittenByOther {
                        by_author: st.last_touch_author,
                        at_action_id: st.last_touch_action_id,
                    },
                });
            }
            let current = current_value(pos);
            let restore_to = st.earliest_targeted_old;
            if cell_eq(&restore_to, &current) {
                return Some(RevertOutcome::Skipped {
                    pos,
                    reason: SkipReason::NoChange,
                });
            }
            Some(RevertOutcome::Restore {
                pos,
                restore_to,
                current,
                earliest_targeted_action_id: st.earliest_targeted_action_id,
                targeted_touch_count: st.targeted_touch_count,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct PointInTimeCutoff {
    pub before_ts: Option<i64>,
    pub before_action_id: Option<u64>,
    pub region: Option<WorldRect>,
}

impl PointInTimeCutoff {
    fn is_past(&self, action: &Action) -> bool {
        self.before_ts.is_some_and(|t| action.ts >= t)
            || self
                .before_action_id
                .is_some_and(|id| action.action_id >= id)
    }

    fn in_region(&self, pos: WorldCoords) -> bool {
        self.region.is_none_or(|r| r.contains(pos))
    }
}

pub fn compute_state_as_of(
    cutoff: &PointInTimeCutoff,
    snapshot_chunks: &[StoredChunk],
    actions_oldest_first: &[Action],
) -> HashMap<WorldCoords, Option<StoredChunkCell>> {
    let mut board: HashMap<WorldCoords, Option<StoredChunkCell>> = HashMap::new();

    for chunk in snapshot_chunks {
        for cell in &chunk.cells {
            let pos = WorldCoords::from_chunk(chunk.coords, cell.local);
            if cutoff.in_region(pos) {
                board.insert(pos, Some(cell.clone()));
            }
        }
    }

    for action in actions_oldest_first {
        if cutoff.is_past(action) {
            break;
        }
        for change in &action.changes {
            if cutoff.in_region(change.pos) {
                board.insert(change.pos, change.new.clone());
            }
        }
    }

    board
}

pub fn compute_point_in_time_revert(
    target_state: &HashMap<WorldCoords, Option<StoredChunkCell>>,
    positions_touched_after_cutoff: impl Iterator<Item = WorldCoords>,
    current_value: impl Fn(WorldCoords) -> Option<StoredChunkCell>,
) -> Vec<RevertOutcome> {
    let mut positions: std::collections::HashSet<WorldCoords> =
        target_state.keys().copied().collect();
    positions.extend(positions_touched_after_cutoff);

    positions
        .into_iter()
        .map(|pos| {
            let restore_to = target_state.get(&pos).cloned().unwrap_or(None);
            let current = current_value(pos);
            if cell_eq(&restore_to, &current) {
                return RevertOutcome::Skipped {
                    pos,
                    reason: SkipReason::NoChange,
                };
            }
            RevertOutcome::Restore {
                pos,
                restore_to,
                current,
                earliest_targeted_action_id: 0,
                targeted_touch_count: 0,
            }
        })
        .collect()
}

pub async fn load_live_action_history(
    state: &crate::state::AppState,
    lower_bound_action_id: Option<u64>,
) -> anyhow::Result<Vec<Action>> {
    let hot: Vec<Action> = {
        let buf = state.inner.hot_buffer.lock().unwrap();
        buf.iter().cloned().collect()
    };

    let oldest_hot_id = hot.first().map(|a| a.action_id);
    let need_disk = match (lower_bound_action_id, oldest_hot_id) {
        (Some(lo), Some(oldest)) => lo < oldest,
        (Some(_), None) => true,
        (None, Some(_)) => true,
        (None, None) => true,
    };

    if !need_disk {
        return Ok(hot);
    }

    let seg_paths = crate::persistence::collect_seg_paths_after(0);
    let mut disk_actions: Vec<Action> = Vec::new();
    for path in &seg_paths {
        let segment = crate::persistence::read_segment_file(path)?;
        disk_actions.extend(segment.actions);
    }
    disk_actions.sort_by_key(|a| a.action_id);

    let oldest_hot_id = hot.first().map(|a| a.action_id).unwrap_or(u64::MAX);
    disk_actions.retain(|a| a.action_id < oldest_hot_id);
    disk_actions.extend(hot);
    Ok(disk_actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use walloftext_shared::ChunkLocalCoords;

    fn pos(x: i16, y: i16) -> WorldCoords {
        WorldCoords { x, y }
    }

    fn cell(ch: char, author_id: u32, ts: i64) -> StoredChunkCell {
        StoredChunkCell {
            local: ChunkLocalCoords { x: 0, y: 0 },
            ch,
            color: walloftext_shared::Rgb(255, 255, 255),
            author_id,
            ts,
        }
    }

    fn action(action_id: u64, author_id: u32, ts: i64, changes: Vec<CellChange>) -> Action {
        Action {
            action_id,
            author_id,
            ts,
            changes,
        }
    }

    fn change(
        p: WorldCoords,
        old: Option<StoredChunkCell>,
        new: Option<StoredChunkCell>,
    ) -> CellChange {
        CellChange { pos: p, old, new }
    }

    #[test]
    fn author_revert_restores_to_before_earliest_targeted_touch() {
        let p = pos(1, 1);
        let actions = vec![
            action(1, 1, 100, vec![change(p, None, Some(cell('X', 1, 100)))]),
            action(
                2,
                1,
                101,
                vec![change(p, Some(cell('X', 1, 100)), Some(cell('Y', 1, 101)))],
            ),
        ];
        let filter = RevertFilter {
            author_id: Some(1),
            ..Default::default()
        };
        let current = |_: WorldCoords| Some(cell('Y', 1, 101));
        let outcomes = compute_revert(&filter, &actions, current);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            RevertOutcome::Restore { restore_to, .. } => assert!(restore_to.is_none()),
            other => panic!("expected Restore, got {other:?}"),
        }
    }

    #[test]
    fn innocent_overwrite_is_preserved_not_stomped() {
        let p = pos(2, 2);
        let legit = cell('A', 5, 100);
        let griefed = cell('X', 1, 101);
        let fixed = cell('B', 5, 102);
        let actions = vec![
            action(1, 5, 100, vec![change(p, None, Some(legit.clone()))]),
            action(
                2,
                1,
                101,
                vec![change(p, Some(legit.clone()), Some(griefed.clone()))],
            ),
            action(
                3,
                5,
                102,
                vec![change(p, Some(griefed.clone()), Some(fixed.clone()))],
            ),
        ];
        let filter = RevertFilter {
            author_id: Some(1),
            ..Default::default()
        };
        let current = |_: WorldCoords| Some(fixed.clone());
        let outcomes = compute_revert(&filter, &actions, current);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            RevertOutcome::Skipped {
                reason: SkipReason::OverwrittenByOther { by_author, .. },
                ..
            } => assert_eq!(*by_author, 5),
            other => panic!("expected Skipped(OverwrittenByOther), got {other:?}"),
        }
    }

    #[test]
    fn attribution_is_preserved_on_restore() {
        let p = pos(3, 3);
        let legit = cell('A', 7, 100);
        let griefed = cell('X', 1, 101);
        let actions = vec![
            action(1, 7, 100, vec![change(p, None, Some(legit.clone()))]),
            action(
                2,
                1,
                101,
                vec![change(p, Some(legit.clone()), Some(griefed.clone()))],
            ),
        ];
        let filter = RevertFilter {
            author_id: Some(1),
            ..Default::default()
        };
        let current = |_: WorldCoords| Some(griefed.clone());
        let outcomes = compute_revert(&filter, &actions, current);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            RevertOutcome::Restore { restore_to, .. } => {
                let restored = restore_to.as_ref().expect("should restore to legit cell");
                assert_eq!(
                    restored.author_id, 7,
                    "attribution must stay with original author, not the acting admin"
                );
                assert_eq!(restored.ch, 'A');
                assert_eq!(restored.ts, 100);
            }
            other => panic!("expected Restore, got {other:?}"),
        }
    }

    #[test]
    fn region_filter_catches_all_authors() {
        let inside = pos(1, 1);
        let outside = pos(100, 100);
        let rect = WorldRect::new(pos(0, 0), pos(10, 10));
        let actions = vec![
            action(
                1,
                1,
                100,
                vec![change(inside, None, Some(cell('X', 1, 100)))],
            ),
            action(
                2,
                2,
                100,
                vec![change(outside, None, Some(cell('X', 2, 100)))],
            ),
        ];
        let filter = RevertFilter {
            region: Some(rect),
            ..Default::default()
        };
        let current = |p: WorldCoords| {
            if p == inside {
                Some(cell('X', 1, 100))
            } else {
                Some(cell('X', 2, 100))
            }
        };
        let outcomes = compute_revert(&filter, &actions, current);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            RevertOutcome::Restore { pos, .. } => assert_eq!(*pos, inside),
            other => panic!("expected Restore for inside pos, got {other:?}"),
        }
    }

    #[test]
    fn combined_author_and_region_filter_is_anded() {
        let p = pos(1, 1);
        let rect = WorldRect::new(pos(0, 0), pos(10, 10));
        let actions = vec![action(
            1,
            2,
            100,
            vec![change(p, None, Some(cell('X', 2, 100)))],
        )];
        let filter = RevertFilter {
            author_id: Some(1),
            region: Some(rect),
            ..Default::default()
        };
        let current = |_: WorldCoords| Some(cell('X', 2, 100));
        let outcomes = compute_revert(&filter, &actions, current);
        assert!(
            outcomes.is_empty(),
            "author mismatch must exclude the change even though region matches"
        );
    }

    #[test]
    fn no_change_skip_when_restore_equals_current() {
        let p = pos(1, 1);
        let actions = vec![action(
            1,
            1,
            100,
            vec![change(p, None, Some(cell('X', 1, 100)))],
        )];
        let filter = RevertFilter {
            author_id: Some(1),
            ..Default::default()
        };
        let current = |_: WorldCoords| None;
        let outcomes = compute_revert(&filter, &actions, current);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            outcomes[0],
            RevertOutcome::Skipped {
                reason: SkipReason::NoChange,
                ..
            }
        ));
    }

    #[test]
    fn range_bounds_targeting_not_scan_window() {
        let p = pos(1, 1);
        let actions = vec![
            action(1, 1, 100, vec![change(p, None, Some(cell('X', 1, 100)))]),
            action(
                2,
                1,
                200,
                vec![change(p, Some(cell('X', 1, 100)), Some(cell('Z', 1, 200)))],
            ),
        ];
        let filter = RevertFilter {
            author_id: Some(1),
            action_id_range: Some((1, 1)),
            ..Default::default()
        };
        let current = |_: WorldCoords| Some(cell('Z', 1, 200));
        let outcomes = compute_revert(&filter, &actions, current);
        assert_eq!(outcomes.len(), 1);
        assert!(
            matches!(
                outcomes[0],
                RevertOutcome::Skipped {
                    reason: SkipReason::OverwrittenByOther { .. },
                    ..
                }
            ),
            "action outside the id range must still count as the last (non-targeted) touch: {:?}",
            outcomes[0]
        );
    }

    #[test]
    fn point_in_time_nulls_positions_created_after_cutoff() {
        let p = pos(4, 4);
        let snapshot_chunks: Vec<StoredChunk> = vec![];
        let actions = vec![action(
            5,
            1,
            500,
            vec![change(p, None, Some(cell('N', 1, 500)))],
        )];
        let cutoff = PointInTimeCutoff {
            before_action_id: Some(5),
            ..Default::default()
        };
        let target = compute_state_as_of(&cutoff, &snapshot_chunks, &actions);
        assert!(
            !target.contains_key(&p),
            "position shouldn't exist in the pre-cutoff scratch board"
        );

        let current = |_: WorldCoords| Some(cell('N', 1, 500));
        let outcomes = compute_point_in_time_revert(&target, std::iter::once(p), current);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            RevertOutcome::Restore { restore_to, .. } => assert!(restore_to.is_none()),
            other => panic!("expected Restore to None, got {other:?}"),
        }
    }

    #[test]
    fn point_in_time_respects_region_scoping() {
        let inside = pos(1, 1);
        let outside = pos(50, 50);
        let rect = WorldRect::new(pos(0, 0), pos(10, 10));
        let snapshot_chunks: Vec<StoredChunk> = vec![];
        let actions = vec![action(
            1,
            1,
            100,
            vec![
                change(inside, None, Some(cell('A', 1, 100))),
                change(outside, None, Some(cell('B', 1, 100))),
            ],
        )];
        let cutoff = PointInTimeCutoff {
            before_action_id: Some(2),
            region: Some(rect),
            ..Default::default()
        };
        let target = compute_state_as_of(&cutoff, &snapshot_chunks, &actions);
        assert!(target.contains_key(&inside));
        assert!(
            !target.contains_key(&outside),
            "out-of-region position must not be seeded"
        );
    }

    #[test]
    fn unbounded_filter_is_rejected() {
        assert!(RevertFilter::default().is_unbounded());
        assert!(
            !RevertFilter {
                author_id: Some(1),
                ..Default::default()
            }
            .is_unbounded()
        );
    }

    #[test]
    fn cap_enforcement_example() {
        let outcomes: Vec<RevertOutcome> = (0..(MAX_REVERT_POSITIONS + 1))
            .map(|i| RevertOutcome::Restore {
                pos: pos((i % 1000) as i16, (i / 1000) as i16),
                restore_to: None,
                current: Some(cell('X', 1, 0)),
                earliest_targeted_action_id: 0,
                targeted_touch_count: 1,
            })
            .collect();
        let restore_count = outcomes
            .iter()
            .filter(|o| matches!(o, RevertOutcome::Restore { .. }))
            .count();
        assert!(restore_count > MAX_REVERT_POSITIONS);
    }
}
