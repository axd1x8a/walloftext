use std::collections::HashMap;
use std::path::Path;

use clap::{Args, Parser, Subcommand};

use walloftext_backend::persistence::{
    collect_seg_paths_after, finalize_segment, read_segment_file, read_snapshot_file,
};
use walloftext_backend::revert::{
    self, PointInTimeCutoff, RevertFilter, RevertOutcome, SkipReason,
};
use walloftext_backend::state::{
    Action, CellChange, SNAPSHOT_PATH, Segment, StoredChunk, StoredChunkCell,
};
use walloftext_shared::{WorldCoords, WorldRect};

#[derive(Parser)]
#[command(name = "admin-revert")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Author(AuthorArgs),
    Region(RegionArgs),
    PointInTime(PointInTimeArgs),
    Inspect(InspectArgs),
}

#[derive(Args)]
struct ApplyArgs {
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    yes: bool,
}

fn parse_region(s: &str) -> Result<WorldRect, String> {
    let parts: Vec<i64> = s
        .split(',')
        .map(|p| p.trim().parse())
        .collect::<Result<_, _>>()
        .map_err(|_| "expected x0,y0,x1,y1".to_string())?;
    if parts.len() != 4 {
        return Err("expected x0,y0,x1,y1".to_string());
    }
    Ok(WorldRect::new(
        WorldCoords::from_i64(parts[0], parts[1]),
        WorldCoords::from_i64(parts[2], parts[3]),
    ))
}

#[derive(Args)]
struct AuthorArgs {
    #[arg(long)]
    author_id: u32,
    #[arg(long, value_parser = parse_region)]
    region: Option<WorldRect>,
    #[arg(long)]
    ts_from: Option<i64>,
    #[arg(long)]
    ts_to: Option<i64>,
    #[arg(long)]
    allow_unbounded: bool,
    #[command(flatten)]
    apply: ApplyArgs,
}

#[derive(Args)]
struct RegionArgs {
    #[arg(long, value_parser = parse_region)]
    region: WorldRect,
    #[arg(long)]
    author_id: Option<u32>,
    #[command(flatten)]
    apply: ApplyArgs,
}

#[derive(Args)]
struct PointInTimeArgs {
    #[arg(long)]
    before_ts: Option<i64>,
    #[arg(long)]
    before_action_id: Option<u64>,
    #[arg(long, value_parser = parse_region)]
    region: Option<WorldRect>,
    #[command(flatten)]
    apply: ApplyArgs,
}

#[derive(Args)]
struct InspectArgs {
    #[arg(long, value_parser = parse_region)]
    region: Option<WorldRect>,
    #[arg(long)]
    ts_from: Option<i64>,
    #[arg(long)]
    ts_to: Option<i64>,
}

struct Board {
    snapshot_chunks: Vec<StoredChunk>,
    actions: Vec<Action>,
    cells: HashMap<WorldCoords, Option<StoredChunkCell>>,
}

fn load_board() -> anyhow::Result<Board> {
    let snap_path = Path::new(SNAPSHOT_PATH);
    let snapshot_chunks = if snap_path.exists() {
        read_snapshot_file(snap_path)?.chunks
    } else {
        Vec::new()
    };

    let seg_paths = collect_seg_paths_after(0);
    let mut actions = Vec::new();
    for path in &seg_paths {
        let segment = read_segment_file(path)?;
        actions.extend(segment.actions);
    }
    actions.sort_by_key(|a| a.action_id);

    let mut cells: HashMap<WorldCoords, Option<StoredChunkCell>> = HashMap::new();
    for chunk in &snapshot_chunks {
        for cell in &chunk.cells {
            let pos = WorldCoords::from_chunk(chunk.coords, cell.local);
            cells.insert(pos, Some(cell.clone()));
        }
    }
    for action in &actions {
        for change in &action.changes {
            cells.insert(change.pos, change.new.clone());
        }
    }

    Ok(Board {
        snapshot_chunks,
        actions,
        cells,
    })
}

fn print_summary(outcomes: &[RevertOutcome]) {
    let mut restore_count = 0;
    let mut skipped_overwritten = 0;
    let mut skipped_nochange = 0;
    let mut author_counts: HashMap<u32, u32> = HashMap::new();

    for o in outcomes {
        match o {
            RevertOutcome::Restore { current, .. } => {
                restore_count += 1;
                if let Some(c) = current {
                    *author_counts.entry(c.author_id).or_insert(0) += 1;
                }
            }
            RevertOutcome::Skipped {
                reason: SkipReason::OverwrittenByOther { .. },
                ..
            } => skipped_overwritten += 1,
            RevertOutcome::Skipped {
                reason: SkipReason::NoChange,
                ..
            } => skipped_nochange += 1,
        }
    }

    eprintln!("restore: {restore_count}");
    eprintln!("skipped (overwritten by other): {skipped_overwritten}");
    eprintln!("skipped (no change): {skipped_nochange}");
    if !author_counts.is_empty() {
        eprintln!("affected authors (cells being restored FROM):");
        let mut pairs: Vec<_> = author_counts.into_iter().collect();
        pairs.sort_by_key(|(a, _)| *a);
        for (author, count) in pairs {
            eprintln!("  author {author}: {count} cell(s)");
        }
    }

    for o in outcomes.iter().take(20) {
        if let RevertOutcome::Restore {
            pos,
            restore_to,
            current,
            ..
        } = o
        {
            let cur = current
                .as_ref()
                .map(|c| format!("'{}' (author {})", c.ch, c.author_id))
                .unwrap_or_else(|| "<empty>".to_string());
            let restore = restore_to
                .as_ref()
                .map(|c| format!("'{}' (author {})", c.ch, c.author_id))
                .unwrap_or_else(|| "<empty>".to_string());
            eprintln!("  ({},{}): {cur} -> {restore}", pos.x, pos.y);
        }
    }
    if restore_count > 20 {
        eprintln!("  ... and {} more", restore_count - 20);
    }
}

fn confirm(apply: &ApplyArgs) -> bool {
    if apply.yes {
        return true;
    }
    eprint!("type 'yes' to apply: ");
    use std::io::Write;
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    line.trim() == "yes"
}

fn apply_offline(board: &Board, outcomes: &[RevertOutcome]) -> anyhow::Result<()> {
    let changes: Vec<CellChange> = outcomes
        .iter()
        .filter_map(|o| match o {
            RevertOutcome::Restore {
                pos, restore_to, ..
            } => Some(CellChange {
                pos: *pos,
                old: board.cells.get(pos).cloned().unwrap_or(None),
                new: restore_to.clone(),
            }),
            _ => None,
        })
        .collect();

    if changes.is_empty() {
        eprintln!("nothing to apply");
        return Ok(());
    }

    let next_action_id = board
        .actions
        .iter()
        .map(|a| a.action_id)
        .max()
        .map(|id| id + 1)
        .unwrap_or(0);

    let seg_paths = collect_seg_paths_after(0);
    let max_segment_id = seg_paths
        .iter()
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()))
        .filter_map(|s| u64::from_str_radix(s, 16).ok())
        .max()
        .unwrap_or(0);

    let action = Action {
        action_id: next_action_id,
        author_id: 0,
        ts: epoch_secs(),
        changes,
    };

    let segment = Segment {
        segment_id: max_segment_id + 1,
        prev_segment_id: if max_segment_id == 0 {
            None
        } else {
            Some(max_segment_id)
        },
        new_users: Default::default(),
        new_regions: Default::default(),
        actions: vec![action],
    };

    finalize_segment(&segment);
    eprintln!(
        "wrote segment {:016x}.seg with {} change(s)",
        segment.segment_id,
        segment.actions[0].changes.len()
    );
    Ok(())
}

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn run_filtered(
    filter: RevertFilter,
    apply: &ApplyArgs,
    allow_unbounded: bool,
) -> anyhow::Result<()> {
    if filter.is_unbounded() && !allow_unbounded {
        eprintln!("filter matches everything; pass --allow-unbounded to proceed");
        std::process::exit(1);
    }

    let board = load_board()?;
    let outcomes = revert::compute_revert(&filter, &board.actions, |pos| {
        board.cells.get(&pos).cloned().unwrap_or(None)
    });

    let restore_count = outcomes
        .iter()
        .filter(|o| matches!(o, RevertOutcome::Restore { .. }))
        .count();
    if restore_count > revert::MAX_REVERT_POSITIONS && !apply.force {
        eprintln!(
            "revert would affect {} positions (cap {}); pass --force to proceed",
            restore_count,
            revert::MAX_REVERT_POSITIONS
        );
        std::process::exit(1);
    }

    print_summary(&outcomes);

    if !apply.apply {
        eprintln!("(dry run; pass --apply to write a corrective segment)");
        return Ok(());
    }

    if !confirm(apply) {
        eprintln!("aborted");
        return Ok(());
    }

    apply_offline(&board, &outcomes)
}

fn run_author(args: AuthorArgs) -> anyhow::Result<()> {
    let filter = RevertFilter {
        author_id: Some(args.author_id),
        region: args.region,
        ts_range: match (args.ts_from, args.ts_to) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        },
        action_id_range: None,
    };
    run_filtered(filter, &args.apply, args.allow_unbounded)
}

fn run_region(args: RegionArgs) -> anyhow::Result<()> {
    let filter = RevertFilter {
        author_id: args.author_id,
        region: Some(args.region),
        ts_range: None,
        action_id_range: None,
    };
    run_filtered(filter, &args.apply, false)
}

fn run_point_in_time(args: PointInTimeArgs) -> anyhow::Result<()> {
    if args.before_ts.is_none() && args.before_action_id.is_none() {
        eprintln!("--before-ts or --before-action-id is required");
        std::process::exit(1);
    }

    let cutoff = PointInTimeCutoff {
        before_ts: args.before_ts,
        before_action_id: args.before_action_id,
        region: args.region,
    };

    let board = load_board()?;
    let target = revert::compute_state_as_of(&cutoff, &board.snapshot_chunks, &board.actions);
    let touched_after: Vec<WorldCoords> = board
        .actions
        .iter()
        .filter(|a| {
            cutoff.before_action_id.is_some_and(|id| a.action_id >= id)
                || cutoff.before_ts.is_some_and(|t| a.ts >= t)
        })
        .flat_map(|a| a.changes.iter().map(|c| c.pos))
        .filter(|pos| cutoff.region.is_none_or(|r| r.contains(*pos)))
        .collect();

    let outcomes =
        revert::compute_point_in_time_revert(&target, touched_after.into_iter(), |pos| {
            board.cells.get(&pos).cloned().unwrap_or(None)
        });

    let restore_count = outcomes
        .iter()
        .filter(|o| matches!(o, RevertOutcome::Restore { .. }))
        .count();
    if restore_count > revert::MAX_REVERT_POSITIONS && !args.apply.force {
        eprintln!(
            "revert would affect {} positions (cap {}); pass --force to proceed",
            restore_count,
            revert::MAX_REVERT_POSITIONS
        );
        std::process::exit(1);
    }

    eprintln!(
        "point-in-time rollback has NO innocent-edit exemption -- this discards everything after the cutoff, by anyone."
    );
    print_summary(&outcomes);

    if !args.apply.apply {
        eprintln!("(dry run; pass --apply to write a corrective segment)");
        return Ok(());
    }

    if !confirm(&args.apply) {
        eprintln!("aborted");
        return Ok(());
    }

    apply_offline(&board, &outcomes)
}

fn run_inspect(args: InspectArgs) -> anyhow::Result<()> {
    let board = load_board()?;
    for action in &board.actions {
        if let Some(from) = args.ts_from
            && action.ts < from
        {
            continue;
        }
        if let Some(to) = args.ts_to
            && action.ts > to
        {
            continue;
        }
        let relevant: Vec<&CellChange> = action
            .changes
            .iter()
            .filter(|c| args.region.is_none_or(|r| r.contains(c.pos)))
            .collect();
        if relevant.is_empty() {
            continue;
        }
        eprintln!(
            "action {} author {} ts {} ({} change(s) in scope)",
            action.action_id,
            action.author_id,
            action.ts,
            relevant.len()
        );
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Author(args) => run_author(args),
        Command::Region(args) => run_region(args),
        Command::PointInTime(args) => run_point_in_time(args),
        Command::Inspect(args) => run_inspect(args),
    }
}
