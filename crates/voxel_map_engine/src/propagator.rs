use bevy::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
#[allow(unused_imports)]
use tracy_client::plot;

use crate::ticket::{LOAD_LEVEL_THRESHOLD, MAX_LEVEL};

/// Maximum BFS steps per propagation call. A player ticket at radius 10
/// affects ~441 columns. 256 handles most single-ticket changes in one frame.
const MAX_BFS_STEPS_PER_FRAME: usize = 64;

/// 8 Chebyshev neighbor offsets on a 2D grid.
const CHEBYSHEV_NEIGHBORS: [IVec2; 8] = [
    IVec2::new(1, 0),
    IVec2::new(-1, 0),
    IVec2::new(0, 1),
    IVec2::new(0, -1),
    IVec2::new(1, 1),
    IVec2::new(1, -1),
    IVec2::new(-1, 1),
    IVec2::new(-1, -1),
];

/// Incremental bucket-queue BFS level propagator.
///
/// Computes per-column effective levels from ticket sources.
/// Propagation uses 2D Chebyshev distance (8-neighbor grid).
/// Updates are incremental: adding/removing/moving a ticket only
/// recomputes the affected region, not the entire map.
///
/// Stored as a component on map entities.
#[derive(Component)]
pub struct TicketLevelPropagator {
    /// Current effective level per column. Contains ALL columns within any
    /// ticket's radius (including levels > LOAD_LEVEL_THRESHOLD).
    levels: HashMap<IVec2, u32>,
    /// Active ticket sources.
    sources: HashMap<Entity, TicketSource>,
    /// Pending BFS updates bucketed by level.
    pending_by_level: BTreeMap<u32, HashSet<IVec2>>,
    /// Whether any sources changed since last propagate().
    dirty: bool,
}

#[derive(Clone, Debug)]
struct TicketSource {
    column: IVec2,
    base_level: u32,
    radius: u32,
}

/// Diff produced by a propagation step, classified by LOAD_LEVEL_THRESHOLD.
///
/// Classification rules:
/// - `loaded`: column was absent or had level > threshold, now has level <= threshold
/// - `changed`: column had level <= threshold before AND after, but level value changed
/// - `unloaded`: column had level <= threshold, now absent or has level > threshold
#[derive(Debug, Default)]
pub struct LevelDiff {
    /// Columns that entered the loaded range (level <= LOAD_LEVEL_THRESHOLD).
    pub loaded: Vec<(IVec2, u32)>,
    /// Columns whose level changed but remained in the loaded range.
    pub changed: Vec<(IVec2, u32)>,
    /// Columns that left the loaded range (should be unloaded).
    pub unloaded: Vec<IVec2>,
}

impl Default for TicketLevelPropagator {
    fn default() -> Self {
        Self::new()
    }
}

impl TicketLevelPropagator {
    /// Creates an empty propagator with no sources or levels.
    pub fn new() -> Self {
        Self {
            levels: HashMap::new(),
            sources: HashMap::new(),
            pending_by_level: BTreeMap::new(),
            dirty: false,
        }
    }

    /// Adds or updates a ticket source. If the entity already has a source,
    /// the old region is invalidated before applying the new one.
    pub fn set_source(&mut self, entity: Entity, column: IVec2, base_level: u32, radius: u32) {
        if let Some(old) = self.sources.remove(&entity) {
            self.queue_invalidation(&old);
        }

        let source = TicketSource {
            column,
            base_level,
            radius,
        };
        self.queue_improvements(&source);
        self.sources.insert(entity, source);
        self.dirty = true;
    }

    /// Removes a ticket source and queues its region for recalculation.
    pub fn remove_source(&mut self, entity: Entity) {
        let Some(source) = self.sources.remove(&entity) else {
            trace!("remove_source: entity {entity:?} has no source, nothing to remove");
            return;
        };
        self.queue_invalidation(&source);
        self.dirty = true;
    }

    /// Runs the BFS propagation, returning the diff of level changes
    /// classified by the load threshold. Processing is amortized: at most
    /// `MAX_BFS_STEPS_PER_FRAME` columns are recomputed per call. The
    /// propagator remains dirty until all pending work is drained.
    pub fn propagate(&mut self) -> LevelDiff {
        if !self.dirty {
            // not dirty, returning empty diff
            return LevelDiff::default();
        }

        let old_loaded = self.snapshot_loaded_columns();
        let steps = self.process_pending_updates(MAX_BFS_STEPS_PER_FRAME);

        // Only mark clean if all pending work is done
        let has_remaining = !self.pending_by_level.is_empty();
        if !has_remaining {
            self.dirty = false;
        }

        plot!("bfs_steps_this_frame", steps as f64);
        plot!("bfs_remaining_dirty", if has_remaining { 1.0 } else { 0.0 });
        plot!(
            "bfs_hit_step_cap",
            if has_remaining && steps >= MAX_BFS_STEPS_PER_FRAME {
                1.0
            } else {
                0.0
            }
        );

        self.build_diff(&old_loaded)
    }

    /// Returns the effective level for a column, if tracked.
    pub fn get_level(&self, col: IVec2) -> Option<u32> {
        self.levels.get(&col).copied()
    }

    /// Returns a reference to all tracked column levels.
    pub fn levels(&self) -> &HashMap<IVec2, u32> {
        &self.levels
    }

    /// Whether any sources have changed since the last propagation.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Returns the minimum Chebyshev distance from this column to any ticket source.
    pub fn min_distance_to_source(&self, col: IVec2) -> u32 {
        self.sources
            .values()
            .map(|s| chebyshev_distance(s.column, col))
            .min()
            .unwrap_or(u32::MAX)
    }

    /// Queues columns in a source's region as potential improvements.
    fn queue_improvements(&mut self, source: &TicketSource) {
        for col in chebyshev_disk(source.column, source.radius) {
            let candidate = candidate_level(source, col);
            let current = self.levels.get(&col).copied().unwrap_or(MAX_LEVEL + 1);
            if candidate < current {
                self.insert_pending(candidate, col);
            }
        }
    }

    /// Queues columns in a source's region for recalculation after removal.
    fn queue_invalidation(&mut self, source: &TicketSource) {
        for col in chebyshev_disk(source.column, source.radius) {
            if let Some(&current) = self.levels.get(&col) {
                self.insert_pending(current, col);
            }
        }
    }

    /// Inserts a column into the pending bucket at the given level.
    fn insert_pending(&mut self, level: u32, col: IVec2) {
        if level <= MAX_LEVEL {
            self.pending_by_level.entry(level).or_default().insert(col);
        }
    }

    /// Snapshots columns currently at or below the load threshold.
    fn snapshot_loaded_columns(&self) -> HashMap<IVec2, u32> {
        self.levels
            .iter()
            .filter(|&(_, lvl)| *lvl <= LOAD_LEVEL_THRESHOLD)
            .map(|(col, lvl)| (*col, *lvl))
            .collect()
    }

    /// Drains pending buckets, recomputing effective levels up to `max_steps`
    /// columns. Returns the number of columns processed. Unprocessed columns
    /// remain in their buckets for the next call.
    fn process_pending_updates(&mut self, max_steps: usize) -> usize {
        let mut steps = 0;
        while steps < max_steps {
            let Some((&level, _)) = self.pending_by_level.first_key_value() else {
                break;
            };
            let columns: Vec<IVec2> = self
                .pending_by_level
                .remove(&level)
                .unwrap()
                .into_iter()
                .collect();
            let mut processed = 0;
            for &col in &columns {
                if steps >= max_steps {
                    break;
                }
                self.recompute_column(col);
                steps += 1;
                processed += 1;
            }
            if processed < columns.len() {
                self.pending_by_level
                    .entry(level)
                    .or_default()
                    .extend(columns[processed..].iter().copied());
            }
        }
        self.pending_by_level.retain(|_, set| !set.is_empty());
        steps
    }

    /// Recomputes the effective level for a single column. If it changed,
    /// queues neighbors for recomputation.
    fn recompute_column(&mut self, col: IVec2) {
        let new_level = self.compute_effective_level(col);
        let old_level = self.levels.get(&col).copied();

        match (old_level, new_level) {
            (Some(old), Some(new)) if old == new => {}
            (None, None) => {}
            (_, Some(new)) => {
                self.levels.insert(col, new);
                self.queue_neighbors(col, new);
            }
            (Some(_), None) => {
                self.levels.remove(&col);
                self.queue_neighbor_recalculation(col);
            }
        }
    }

    /// Computes the minimum level for a column across all sources.
    /// Returns `None` if no source covers this column.
    fn compute_effective_level(&self, col: IVec2) -> Option<u32> {
        let mut best: Option<u32> = None;
        for source in self.sources.values() {
            let dist = chebyshev_distance(source.column, col);
            if dist > source.radius {
                continue;
            }
            let level = source.base_level + dist;
            if level > MAX_LEVEL {
                continue;
            }
            best = Some(best.map_or(level, |b: u32| b.min(level)));
        }
        best
    }

    /// Queues the 8 Chebyshev neighbors as potential improvements from a
    /// column that just got a new level.
    fn queue_neighbors(&mut self, col: IVec2, new_level: u32) {
        if new_level >= MAX_LEVEL {
            return;
        }
        let neighbor_candidate = new_level + 1;
        for offset in CHEBYSHEV_NEIGHBORS {
            let neighbor = col + offset;
            let current = self.levels.get(&neighbor).copied().unwrap_or(MAX_LEVEL + 1);
            if neighbor_candidate < current {
                self.insert_pending(neighbor_candidate, neighbor);
            } else {
                self.insert_pending(current.min(MAX_LEVEL), neighbor);
            }
        }
    }

    /// Queues neighbors of a removed column for recalculation at their current level.
    fn queue_neighbor_recalculation(&mut self, col: IVec2) {
        for offset in CHEBYSHEV_NEIGHBORS {
            let neighbor = col + offset;
            if let Some(&current) = self.levels.get(&neighbor) {
                self.insert_pending(current, neighbor);
            }
        }
    }

    /// Compares old loaded snapshot to current levels and classifies changes.
    fn build_diff(&self, old_loaded: &HashMap<IVec2, u32>) -> LevelDiff {
        let mut diff = LevelDiff::default();

        self.classify_current_columns(old_loaded, &mut diff);
        self.classify_removed_columns(old_loaded, &mut diff);

        diff
    }

    /// Finds columns that are now loaded or changed relative to old snapshot.
    fn classify_current_columns(&self, old_loaded: &HashMap<IVec2, u32>, diff: &mut LevelDiff) {
        for (col, new_lvl) in &self.levels {
            if *new_lvl > LOAD_LEVEL_THRESHOLD {
                continue;
            }
            match old_loaded.get(col) {
                Some(old_lvl) if old_lvl == new_lvl => {}
                Some(_) => diff.changed.push((*col, *new_lvl)),
                None => diff.loaded.push((*col, *new_lvl)),
            }
        }
    }

    /// Finds columns that were loaded but are now absent or above threshold.
    fn classify_removed_columns(&self, old_loaded: &HashMap<IVec2, u32>, diff: &mut LevelDiff) {
        for col in old_loaded.keys() {
            let still_loaded = self
                .levels
                .get(col)
                .is_some_and(|lvl| *lvl <= LOAD_LEVEL_THRESHOLD);
            if !still_loaded {
                diff.unloaded.push(*col);
            }
        }
    }
}

/// Chebyshev (L-infinity) distance between two 2D points.
fn chebyshev_distance(a: IVec2, b: IVec2) -> u32 {
    let d = (a - b).abs();
    d.x.max(d.y) as u32
}

/// Candidate level for a column given a source.
fn candidate_level(source: &TicketSource, col: IVec2) -> u32 {
    source.base_level + chebyshev_distance(source.column, col)
}

/// Iterates all columns within Chebyshev radius of a center.
fn chebyshev_disk(center: IVec2, radius: u32) -> impl Iterator<Item = IVec2> {
    let r = radius as i32;
    (-r..=r).flat_map(move |dx| (-r..=r).map(move |dz| center + IVec2::new(dx, dz)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(id: u32) -> Entity {
        Entity::from_raw_u32(id).expect("valid test entity")
    }

    /// Runs propagation to completion across multiple amortized calls.
    /// Returns the final loaded set (not accumulated partial diffs, which
    /// can contain duplicates — e.g., a column "loaded" in diff 1 then
    /// "changed" in diff 2).
    fn propagate_fully(prop: &mut TicketLevelPropagator) -> LevelDiff {
        while prop.is_dirty() {
            prop.propagate();
        }
        // Build diff from final state vs empty (everything is "loaded")
        let mut diff = LevelDiff::default();
        for (&col, &level) in prop.levels() {
            if level <= LOAD_LEVEL_THRESHOLD {
                diff.loaded.push((col, level));
            }
        }
        diff
    }

    #[test]
    fn single_player_ticket_produces_concentric_levels() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 0, 5);
        let diff = propagate_fully(&mut prop);

        assert_eq!(prop.get_level(IVec2::ZERO), Some(0));
        assert_eq!(prop.get_level(IVec2::new(1, 0)), Some(1));
        assert_eq!(prop.get_level(IVec2::new(1, 1)), Some(1));
        assert_eq!(prop.get_level(IVec2::new(2, 0)), Some(2));
        assert_eq!(prop.get_level(IVec2::new(5, 0)), Some(5));
        assert_eq!(prop.get_level(IVec2::new(6, 0)), None);

        let loaded_count = diff.loaded.len();
        // LOAD_LEVEL_THRESHOLD=4 (cfg(test)), base_level=0, radius=5 → loaded at distance ≤ 4 = (2*4+1)² = 81
        assert_eq!(loaded_count, 81);
    }

    #[test]
    fn npc_ticket_starts_at_level_1() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 1, 3);
        prop.propagate();

        assert_eq!(prop.get_level(IVec2::ZERO), Some(1));
        assert_eq!(prop.get_level(IVec2::X), Some(2));
        assert_eq!(prop.get_level(IVec2::new(2, 0)), Some(3));
    }

    #[test]
    fn overlapping_tickets_take_minimum_level() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 0, 5);
        prop.set_source(entity(2), IVec2::new(3, 0), 1, 3);
        prop.propagate();

        assert_eq!(prop.get_level(IVec2::new(3, 0)), Some(1));
        assert_eq!(prop.get_level(IVec2::ZERO), Some(0));
        assert_eq!(prop.get_level(IVec2::new(2, 0)), Some(2));
    }

    #[test]
    fn ticket_removal_unloads_exclusive_columns() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 0, 2);
        propagate_fully(&mut prop);

        assert_eq!(prop.get_level(IVec2::new(2, 0)), Some(2));

        prop.remove_source(entity(1));
        let mut total_unloaded = Vec::new();
        while prop.is_dirty() {
            let diff = prop.propagate();
            total_unloaded.extend(diff.unloaded);
        }

        assert_eq!(prop.get_level(IVec2::ZERO), None);
        assert_eq!(total_unloaded.len(), 25);
    }

    #[test]
    fn ticket_move_updates_levels_incrementally() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 0, 3);
        prop.propagate();

        assert_eq!(prop.get_level(IVec2::ZERO), Some(0));

        prop.set_source(entity(1), IVec2::new(5, 0), 0, 3);
        let diff = prop.propagate();

        assert_eq!(prop.get_level(IVec2::ZERO), None);
        assert_eq!(prop.get_level(IVec2::new(5, 0)), Some(0));

        assert!(!diff.loaded.is_empty());
        assert!(!diff.unloaded.is_empty());
    }

    #[test]
    fn ticket_move_one_step_has_minimal_diff() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 0, 3);
        prop.propagate();

        prop.set_source(entity(1), IVec2::X, 0, 3);
        let diff = prop.propagate();

        let total_changes = diff.loaded.len() + diff.changed.len() + diff.unloaded.len();
        assert!(
            total_changes < 49,
            "expected incremental diff, got {total_changes} changes"
        );
    }

    #[test]
    fn no_propagation_when_clean() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 0, 2);
        prop.propagate();

        let diff = prop.propagate();
        assert!(diff.loaded.is_empty());
        assert!(diff.changed.is_empty());
        assert!(diff.unloaded.is_empty());
    }

    #[test]
    fn chebyshev_distance_correct_for_diagonals() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 0, 5);
        propagate_fully(&mut prop);

        assert_eq!(prop.get_level(IVec2::new(3, 3)), Some(3));
        assert_eq!(prop.get_level(IVec2::new(2, 4)), Some(4));
    }

    #[test]
    fn diff_classifies_by_load_threshold() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 0, 3);
        let diff = propagate_fully(&mut prop);

        // LOAD_LEVEL_THRESHOLD=4 (cfg(test)), base_level=0, radius=3 → all 7x7=49 at level ≤ 3 are loaded
        assert_eq!(diff.loaded.len(), 49);
        assert!(diff.changed.is_empty());
        assert!(diff.unloaded.is_empty());

        // Distance 3, level 3 ≤ threshold 4 → IS in diff.loaded
        assert!(diff.loaded.iter().any(|(col, _)| *col == IVec2::new(3, 0)));
        assert_eq!(prop.get_level(IVec2::new(3, 0)), Some(3));
    }

    #[test]
    fn overlapping_ticket_strengthens_column_produces_changed() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 0, 3);
        prop.propagate();

        assert_eq!(prop.get_level(IVec2::new(2, 0)), Some(2));

        prop.set_source(entity(2), IVec2::new(2, 0), 0, 2);
        let diff = prop.propagate();

        assert!(
            diff.changed
                .iter()
                .any(|(col, lvl)| *col == IVec2::new(2, 0) && *lvl == 0)
        );
    }

    #[test]
    fn column_crossing_threshold_is_loaded_or_unloaded() {
        let mut prop = TicketLevelPropagator::new();
        // NPC base_level=1, radius=3 → distance 2 = level 3 (≤ threshold 4, loaded)
        prop.set_source(entity(1), IVec2::ZERO, 1, 3);
        prop.propagate();

        assert_eq!(prop.get_level(IVec2::new(2, 0)), Some(3));

        // Adding player base_level=0 → distance 2 = level 2, strengthens from 3 to 2
        // Both ≤ threshold → "changed", not "loaded"
        prop.set_source(entity(2), IVec2::ZERO, 0, 3);
        let diff = prop.propagate();

        assert!(
            diff.changed
                .iter()
                .any(|(col, lvl)| *col == IVec2::new(2, 0) && *lvl == 2)
        );
    }

    #[test]
    fn large_radius_column_count() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 0, 10);
        propagate_fully(&mut prop);

        let count = prop.levels().len();
        assert_eq!(count, 441);
    }

    #[test]
    fn removal_with_overlapping_ticket_preserves_shared_columns() {
        let mut prop = TicketLevelPropagator::new();
        prop.set_source(entity(1), IVec2::ZERO, 0, 3);
        prop.set_source(entity(2), IVec2::new(2, 0), 0, 3);
        propagate_fully(&mut prop);

        prop.remove_source(entity(1));
        let diff = prop.propagate();

        assert_eq!(prop.get_level(IVec2::ZERO), Some(2));
        assert!(diff.changed.iter().any(|(col, _)| *col == IVec2::ZERO));
        assert!(diff.unloaded.iter().any(|col| *col == IVec2::new(-2, 0)));
    }
}
