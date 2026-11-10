//! Port of `src/QuickByte.Core/Services/FileMerger.cs`.
//!
//! Merges the ordered chunk files written by each connection into the single
//! final file, deleting each chunk as soon as its bytes are safely in the output.
//!
//! **Why it deletes as it goes.** Merging used to copy every chunk into a new
//! file and delete them all at the end, so for the length of the merge a
//! download occupied twice its size. A 40 GB file on a drive with 50 GB free
//! downloaded to 100% and then failed in the merge — the most expensive moment
//! there is to run out of disk.
//!
//! **Why it merges in place.** When the temp folder and the destination share a
//! volume, the chunks are appended onto the first one, which is then renamed
//! into place. Beyond the chunks themselves the merge needs room for only the
//! chunk being appended, and a single-connection download's "merge" is a rename
//! — it used to copy the whole file. On another volume the chunks are copied into
//! the destination instead, still deleting each one as it lands.
//!
//! **Why it keeps a plan.** A merge that deletes its inputs cannot be started
//! over after an interruption — the app exiting half way through a 40 GB merge is
//! ordinary. So before it changes anything it writes [`PLAN_FILE_NAME`]: the
//! chunks in order, with their lengths. The output's length then says exactly
//! how far it got: every chunk whose end it reaches is merged (and deleted, or
//! about to be), and anything past the last such end is a partial append, cut
//! off and done again. Each chunk is flushed to disk before it is deleted, so the
//! bytes are never only in a write cache.
//!
//! **Schema.** `merge.json` is the C# record as System.Text.Json wrote it:
//! `{"Destination":"D:\\x.iso","InPlace":true,"Chunks":[{"Name":"part0.tmp","Length":5}]}`.
//!
//! The work is file I/O from start to finish, so it runs on tokio's blocking
//! pool rather than on an async worker; cancellation is checked between reads.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, Result};
use crate::helpers::disk_space;
use crate::traits::{self, ProgressFn};

pub const PLAN_FILE_NAME: &str = "merge.json";

/// Merging runs at disk speed, so reporting every buffer would post thousands of
/// UI updates per second and stall the very windows it is meant to inform. One
/// report per this many merged bytes (plus a final exact one) is plenty for a
/// smooth bar.
const PROGRESS_REPORT_INTERVAL_BYTES: i64 = 4 * 1024 * 1024;

const BROKEN_MESSAGE: &str = "Part of this download is no longer on disk, so it has to be downloaded again. Click Retry.";

/// How much space a volume has.
pub type AvailableBytesFn = Arc<dyn Fn(&str) -> Option<i64> + Send + Sync>;
/// Whether two paths share a volume.
pub type SameVolumeFn = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Plan {
    pub destination: String,
    /// Appending onto the first chunk and renaming it, rather than copying into the destination.
    pub in_place: bool,
    pub chunks: Vec<PlanChunk>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PlanChunk {
    pub name: String,
    pub length: i64,
}

impl PlanChunk {
    pub fn new(name: impl Into<String>, length: i64) -> Self {
        Self { name: name.into(), length }
    }
}

#[derive(Clone)]
pub struct FileMerger {
    available_bytes: AvailableBytesFn,
    same_volume: SameVolumeFn,
}

impl Default for FileMerger {
    fn default() -> Self {
        Self::new()
    }
}

impl FileMerger {
    pub fn new() -> Self {
        Self::with_disk(Arc::new(disk_space::available_bytes), Arc::new(disk_space::same_volume))
    }

    /// For the tests: how much space a volume has, and whether two paths share
    /// one, are the two facts about the machine this type acts on, and neither
    /// can be arranged on demand.
    pub fn with_disk(available_bytes: AvailableBytesFn, same_volume: SameVolumeFn) -> Self {
        Self { available_bytes, same_volume }
    }

    /// The merge itself, on the calling thread.
    pub fn merge_blocking(
        &self,
        temp_folder: &str,
        ordered_chunk_paths: &[String],
        destination_file_path: &str,
        buffer_size: usize,
        bytes_merged: Option<&ProgressFn<i64>>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let report = |merged: i64| {
            if let Some(report) = bytes_merged {
                report(merged);
            }
        };

        let plan = read_plan(temp_folder);
        if plan.as_ref().is_some_and(|p| !same_path(&p.destination, destination_file_path)) {
            return Err(broken(temp_folder, destination_file_path));
        }

        // Nothing to merge from: a plan file that could not be read, or a resume
        // that brought no chunks. Starting a fresh merge of nothing would produce
        // an empty file and call it finished.
        if plan.is_none() && (has_interrupted_merge(temp_folder) || ordered_chunk_paths.is_empty()) {
            return Err(broken(temp_folder, destination_file_path));
        }

        let plan = match plan {
            Some(plan) => plan,
            None => self.begin(temp_folder, ordered_chunk_paths, destination_file_path)?,
        };

        let total: i64 = plan.chunks.iter().map(|c| c.length).sum();
        let chunk_at = |i: usize| Path::new(temp_folder).join(&plan.chunks[i].name);
        let target: PathBuf = if plan.in_place { chunk_at(0) } else { PathBuf::from(&plan.destination) };

        if plan.in_place && !target.is_file() {
            // Interrupted after the rename and before the plan was deleted: the
            // file is already where it belongs.
            if file_length(Path::new(&plan.destination)) == Some(total) {
                try_delete(&plan_path(temp_folder));
                report(total);
                return Ok(());
            }
            return Err(broken(temp_folder, destination_file_path));
        }

        let mut merged;
        {
            let mut output = OpenOptions::new().write(true).create(true).truncate(false).open(&target)?;
            let output_length = output.metadata()?.len() as i64;
            let Some((next, already)) = resume_point(&plan, output_length) else {
                drop(output);
                return Err(broken(temp_folder, destination_file_path));
            };
            merged = already;

            // A partial append past the last whole chunk is cut off and done again.
            output.set_len(merged as u64)?;
            output.seek(SeekFrom::Start(merged as u64))?;

            // Chunks already in the output whose delete was interrupted.
            for i in (if plan.in_place { 1 } else { 0 })..next {
                try_delete(&chunk_at(i));
            }

            for (i, chunk) in plan.chunks.iter().enumerate().skip(next) {
                if file_length(&chunk_at(i)) != Some(chunk.length) {
                    drop(output);
                    return Err(broken(temp_folder, destination_file_path));
                }
            }

            self.ensure_room(&plan, next, merged, &target.to_string_lossy())?;

            report(merged);
            let mut last_reported = merged;

            // One buffer for the whole merge rather than one per chunk: nothing
            // survives an iteration.
            let mut buffer = vec![0u8; buffer_size.max(1)];

            for i in next..plan.chunks.len() {
                if cancel.is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                let chunk_path = chunk_at(i);
                {
                    let mut source = File::open(&chunk_path)?;
                    loop {
                        if cancel.is_cancelled() {
                            return Err(CoreError::Cancelled);
                        }
                        let read = match source.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e) => return Err(e.into()),
                        };
                        output.write_all(&buffer[..read])?;
                        merged += read as i64;
                        if merged - last_reported >= PROGRESS_REPORT_INTERVAL_BYTES {
                            last_reported = merged;
                            report(merged);
                        }
                    }
                }

                // Durable before the only other copy of these bytes is deleted.
                output.flush()?;
                output.sync_all()?;
                try_delete(&chunk_path);
            }
        }

        if plan.in_place {
            if let Some(parent) = Path::new(&plan.destination).parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&target, &plan.destination)?;
        }

        try_delete(&plan_path(temp_folder));
        report(merged); // final, exact
        Ok(())
    }

    /// Records what is about to be merged, before anything is changed.
    fn begin(&self, temp_folder: &str, ordered_chunk_paths: &[String], destination_file_path: &str) -> Result<Plan> {
        let destination = full_path(destination_file_path).ok_or_else(|| CoreError::invalid(format!("'{destination_file_path}' is not a file path.")))?;
        if let Some(parent) = Path::new(&destination).parent() {
            fs::create_dir_all(parent)?;
        }

        let mut chunks = Vec::with_capacity(ordered_chunk_paths.len());
        for path in ordered_chunk_paths {
            let full = full_path(path).unwrap_or_else(|| path.clone());
            let folder = Path::new(&full).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            if !same_path(&folder, temp_folder) {
                return Err(CoreError::invalid(format!("Chunk '{path}' is not inside '{temp_folder}'.")));
            }
            let name = Path::new(&full).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            chunks.push(PlanChunk::new(name, fs::metadata(&full)?.len() as i64));
        }

        let plan = Plan { in_place: !chunks.is_empty() && (self.same_volume)(temp_folder, &destination), destination, chunks };

        self.ensure_room(&plan, if plan.in_place { 1 } else { 0 }, 0, &plan.destination)?;

        // On another volume the output is the destination itself, so a file
        // already there would read as merged bytes on a resume. Cleared before the
        // plan exists, never after.
        if !plan.in_place {
            File::create(&plan.destination)?;
        }

        write_plan(temp_folder, &plan)?;
        Ok(plan)
    }

    fn ensure_room(&self, plan: &Plan, next: usize, merged: i64, target: &str) -> Result<()> {
        // In place, only the chunk being appended is ever held twice; elsewhere
        // the rest of the file has to fit at the destination.
        let needed = if plan.in_place {
            plan.chunks.iter().skip(next).map(|c| c.length).max().unwrap_or(0)
        } else {
            plan.chunks.iter().map(|c| c.length).sum::<i64>() - merged
        };
        let available = Arc::clone(&self.available_bytes);
        match disk_space::find_shortfall([(target, needed)], |p| available(p)) {
            Some(shortfall) => Err(CoreError::InsufficientDiskSpace(shortfall)),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl traits::FileMerger for FileMerger {
    async fn merge(
        &self,
        temp_folder: &str,
        ordered_chunk_paths: &[String],
        destination_file_path: &str,
        buffer_size: usize,
        bytes_merged: Option<ProgressFn<i64>>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let merger = self.clone();
        let temp_folder = temp_folder.to_string();
        let chunks = ordered_chunk_paths.to_vec();
        let destination = destination_file_path.to_string();
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            merger.merge_blocking(&temp_folder, &chunks, &destination, buffer_size, bytes_merged.as_ref(), &cancel)
        })
        .await
        .map_err(|e| CoreError::other(format!("The merge stopped unexpectedly: {e}")))?
    }

    fn has_interrupted_merge(&self, temp_folder: &str) -> bool {
        has_interrupted_merge(temp_folder)
    }

    fn discard_interrupted_merge(&self, temp_folder: &str, destination_file_path: &str) {
        discard_interrupted_merge(temp_folder, destination_file_path);
    }

    fn cleanup_chunks(&self, chunk_paths: &[String], temp_folder: &str) {
        for path in chunk_paths {
            // Best-effort cleanup, ignore locked/missing files.
            try_delete(Path::new(path));
        }
        let folder = Path::new(temp_folder);
        if !temp_folder.is_empty() && fs::read_dir(folder).is_ok_and(|mut entries| entries.next().is_none()) {
            let _ = fs::remove_dir(folder);
        }
    }
}

pub fn has_interrupted_merge(temp_folder: &str) -> bool {
    !temp_folder.trim().is_empty() && plan_path(temp_folder).is_file()
}

pub fn discard_interrupted_merge(temp_folder: &str, destination_file_path: &str) {
    if !has_interrupted_merge(temp_folder) {
        return;
    }
    // Only ever the file this download names. The plan is read from disk, and a
    // path read from a file is not one to delete on that file's word alone.
    if let Some(plan) = read_plan(temp_folder)
        && !plan.in_place
        && same_path(&plan.destination, destination_file_path)
    {
        try_delete(Path::new(destination_file_path));
    }
    try_delete(&plan_path(temp_folder));
}

/// The first chunk still to merge, given what is already in the output, and the
/// length of output that belongs to the chunks before it. `None` when the output
/// is shorter than a merge in place could ever leave it: its first chunk was
/// complete when the plan was written.
fn resume_point(plan: &Plan, output_length: i64) -> Option<(usize, i64)> {
    let mut merged = 0;
    let mut next = 0;
    for chunk in &plan.chunks {
        if merged + chunk.length > output_length {
            break;
        }
        merged += chunk.length;
        next += 1;
    }
    if plan.in_place && next == 0 { None } else { Some((next, merged)) }
}

/// A merge whose inputs are not what the plan says. Nothing can be salvaged by
/// trying again, so the plan is forgotten and the next run downloads what is
/// missing — the pool recognises a first chunk that grew past its range and
/// starts the set over.
fn broken(temp_folder: &str, destination_file_path: &str) -> CoreError {
    discard_interrupted_merge(temp_folder, destination_file_path);
    CoreError::Io(BROKEN_MESSAGE.into())
}

fn plan_path(temp_folder: &str) -> PathBuf {
    Path::new(temp_folder).join(PLAN_FILE_NAME)
}

fn read_plan(temp_folder: &str) -> Option<Plan> {
    let bytes = fs::read(plan_path(temp_folder)).ok()?;
    let plan: Plan = serde_json::from_slice(crate::serde_compat::strip_bom(&bytes)).ok()?;
    (!plan.chunks.is_empty() && plan.chunks.iter().all(is_safe_chunk)).then_some(plan)
}

/// A chunk name that is a bare file name with a real length, not a path out of the folder.
fn is_safe_chunk(chunk: &PlanChunk) -> bool {
    chunk.length >= 0
        && !chunk.name.trim().is_empty()
        && Path::new(&chunk.name).file_name().is_some_and(|n| n.to_string_lossy() == chunk.name)
}

/// Written beside the plan and renamed over it, so a crash never leaves half a plan.
pub fn write_plan(temp_folder: &str, plan: &Plan) -> Result<()> {
    fs::create_dir_all(temp_folder)?;
    let path = plan_path(temp_folder);
    let pending = Path::new(temp_folder).join(format!("{PLAN_FILE_NAME}.pending"));
    {
        let mut file = File::create(&pending)?;
        file.write_all(&serde_json::to_vec(plan)?)?;
        file.sync_all()?;
    }
    fs::rename(&pending, &path)?;
    Ok(())
}

/// `Path.GetFullPath`.
pub(crate) fn full_path(path: &str) -> Option<String> {
    if path.trim().is_empty() {
        return None;
    }
    std::path::absolute(path).ok().map(|p| p.to_string_lossy().into_owned())
}

/// The same file, by full path, ignoring case and a trailing separator.
pub(crate) fn same_path(a: &str, b: &str) -> bool {
    match (full_path(a), full_path(b)) {
        (Some(a), Some(b)) => {
            let trim = |s: &str| s.trim_end_matches(['\\', '/']).to_lowercase();
            trim(&a) == trim(&b)
        }
        _ => false,
    }
}

fn file_length(path: &Path) -> Option<i64> {
    fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| m.len() as i64)
}

/// Best-effort — a later pass or the sweep collects it.
fn try_delete(path: &Path) {
    if path.is_file() {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests;
