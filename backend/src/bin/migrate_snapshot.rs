use walloftext_backend::state::WorldSnapshot;

struct Migration {
    name: &'static str,
    apply: fn(WorldSnapshot) -> Option<WorldSnapshot>,
}

const MIGRATIONS: &[Migration] = &[Migration {
    name: "fix UID drift",
    apply: shift_uids_minus_1,
}];

fn shift_uids_minus_1(mut snap: WorldSnapshot) -> Option<WorldSnapshot> {
    let system_at_1 = snap
        .users
        .iter()
        .any(|u| u.user_id == 1 && u.username == "system");
    if !system_at_1 {
        return None;
    }

    snap.next_uid -= 1;

    for u in &mut snap.users {
        u.user_id -= 1;
    }

    for r in &mut snap.regions {
        if r.owner_id > 0 {
            r.owner_id -= 1;
        }
    }

    for chunk in &mut snap.chunks {
        for cell in &mut chunk.cells {
            if cell.author_id > 0 {
                cell.author_id -= 1;
            }
        }
    }

    if snap.authors_count.len() >= 2 {
        let merged = snap.authors_count[0] + snap.authors_count[1];
        snap.authors_count.remove(0);
        snap.authors_count[0] = merged;
    } else if snap.authors_count.len() == 1 {
        snap.authors_count.remove(0);
    }

    Some(snap)
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .unwrap_or_else(|| "data/walloftext.snap".to_string());
    let output = args
        .next()
        .unwrap_or_else(|| "data/walloftext.snap".to_string());

    eprintln!("Reading snapshot from: {input}");
    let compressed = std::fs::read(&input)?;
    let raw = zstd::decode_all(compressed.as_slice())?;
    let mut snap: WorldSnapshot =
        bitcode::decode(&raw).map_err(|e| anyhow::anyhow!("decode failed: {e}"))?;

    eprintln!(
        "Loaded: {} users, {} regions, {} chunks",
        snap.users.len(),
        snap.regions.len(),
        snap.chunks.len(),
    );

    let mut applied = 0usize;
    for migration in MIGRATIONS {
        match (migration.apply)(snap.clone()) {
            Some(new_snap) => {
                eprintln!("  [+] applied: {}", migration.name);
                snap = new_snap;
                applied += 1;
            }
            None => {
                eprintln!("  [-] skipped: {}", migration.name);
            }
        }
    }

    if applied == 0 {
        eprintln!("No migrations needed, snapshot is up to date.");
        return Ok(());
    }

    eprintln!("Writing migrated snapshot to: {output}");
    let encoded = bitcode::encode(&snap);
    let compressed_out = zstd::encode_all(encoded.as_slice(), 3)?;
    let tmp = format!("{output}.tmp");
    std::fs::write(&tmp, &compressed_out)?;
    std::fs::rename(&tmp, &output)?;
    eprintln!("Done ({applied} migration(s) applied).");
    Ok(())
}
