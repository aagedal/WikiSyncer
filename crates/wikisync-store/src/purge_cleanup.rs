//! Restartable physical cleanup for authenticated destructive-purge journals.
//!
//! Every public entry point revalidates the typed manifest event before it can
//! advance into logical absence or file retirement. Mixed packs are rewritten one
//! at a time with exactly their retained object subset before the old representation
//! can become eligible for retirement.

use super::*;

/// Largest unfinished-purge page accepted by the cleanup discovery API.
pub const MAX_UNFINISHED_PURGE_PAGE_SIZE: u32 = 1_000;

/// Observable checkpoint completed by one cleanup-driver call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PurgeCleanupStep {
    /// The purge event and exact physical retirement inventory are durable.
    Prepared,
    /// One whole-pack work item was verified and needs no replacement.
    WholePackReady,
    /// Compatibility marker for an external replacement boundary. The built-in
    /// cleanup driver now performs bounded replacement and does not emit this step.
    ReplacementRequired,
    /// One mixed pack's exact retained subset was durably installed and activated.
    ReplacementReady,
    /// Logical absence and derived-index removal committed atomically.
    AuthorizedAbsenceCommitted,
    /// One loose file or old pack/index group was retired and its directory synced.
    FilesRetired,
    /// Accounting and the terminal journal state committed.
    Completed,
    /// The journal had already reached its terminal successful state.
    AlreadyComplete,
}

/// Bounded durable progress for one authenticated cleanup journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PurgeCleanupProgress {
    pub purge_id: u64,
    pub state: PurgeJournalState,
    pub manifest_installed: bool,
    pub pending_pack_count: u64,
    pub replacement_ready_pack_count: u64,
    pub retired_pack_count: u64,
    pub pending_file_count: u64,
    pub unlinking_file_count: u64,
    pub retired_file_count: u64,
    pub retired_file_bytes: u64,
    pub replacement_file_bytes: u64,
    /// Positive means managed files became smaller; negative means replacement
    /// files currently occupy more bytes than retired files reclaimed.
    pub net_reclaimed_file_bytes: i64,
}

/// Result of one bounded, cancellation-safe cleanup advance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PurgeCleanupAdvance {
    pub step: PurgeCleanupStep,
    pub progress: PurgeCleanupProgress,
}

/// One exact logical object whose missing payload is authorized by a purge journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PurgeAuthorizedAbsence {
    pub purge_id: u64,
    pub object: StoredObject,
    pub absent_at: u64,
    pub superseded_at: Option<u64>,
}

#[derive(Debug)]
struct LooseFileWork {
    relative_path: String,
    location_id: i64,
    object_id: ObjectId,
    expected_file_bytes: u64,
    state: String,
}

#[derive(Debug)]
struct PackFileWork {
    file_kind: String,
    relative_path: String,
    expected_checksum: String,
    expected_file_bytes: u64,
    state: String,
}

impl Library {
    /// Verifies phase-specific cleanup invariants before integrity code accepts an
    /// authorized absence or reports a purge as complete.
    pub fn verify_purge_cleanup_state(
        &self,
        purge_id: u64,
    ) -> Result<PurgeCleanupProgress, StoreError> {
        let event = self.validate_authenticated_purge(purge_id)?;
        let progress = self.purge_cleanup_progress(purge_id)?;
        let raw_purge_id = to_sql_integer(purge_id)?;
        let expected_file_count = event
            .affected_pack_count
            .checked_mul(2)
            .and_then(|count| count.checked_add(event.loose_object_count))
            .ok_or(StoreError::PurgeLimitExceeded)?;
        let total_file_count = progress
            .pending_file_count
            .checked_add(progress.unlinking_file_count)
            .and_then(|count| count.checked_add(progress.retired_file_count))
            .ok_or(StoreError::PurgeLimitExceeded)?;
        let absence_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM purge_authorized_absences WHERE purge_id = ?1",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        let absence_count = sql_u64(absence_count, "invalid authorized absence count")?;
        let verified_target_locations: i64 = if progress.state == PurgeJournalState::Succeeded {
            self.connection.query_row(
                "SELECT COUNT(*) FROM object_locations AS location
                 JOIN purge_authorized_absences AS absence
                   ON absence.purge_id = ?1
                  AND absence.object_id = location.object_id
                  AND absence.superseded_at IS NULL
                 WHERE location.verification_state = 'verified'",
                [raw_purge_id],
                |row| row.get(0),
            )?
        } else {
            self.connection.query_row(
                "SELECT COUNT(*) FROM object_locations AS location
                 JOIN purge_objects AS selected
                   ON selected.purge_id = ?1
                  AND selected.object_id = location.object_id
                 WHERE location.verification_state = 'verified'",
                [raw_purge_id],
                |row| row.get(0),
            )?
        };
        let verified_target_locations = sql_u64(
            verified_target_locations,
            "invalid verified purge target location count",
        )?;
        let finished_at: Option<i64> = self.connection.query_row(
            "SELECT finished_at FROM purge_operations WHERE purge_id = ?1",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        type Accounting = (i64, i64, Option<i64>, Option<i64>);
        let accounting: Option<Accounting> = self
            .connection
            .query_row(
                "SELECT retired_file_bytes, replacement_file_bytes,
                        directories_synced_at, completed_at
                 FROM purge_cleanup_accounting WHERE purge_id = ?1",
                [raw_purge_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;

        match progress.state {
            PurgeJournalState::Authorized => {
                if total_file_count != 0
                    || absence_count != 0
                    || accounting.is_some()
                    || finished_at.is_some()
                {
                    return Err(StoreError::CorruptMetadata(
                        "authorized purge has premature cleanup state",
                    ));
                }
                return Ok(progress);
            }
            PurgeJournalState::Repacking => {
                if total_file_count != expected_file_count
                    || progress.unlinking_file_count != 0
                    || progress.retired_file_count != 0
                    || progress.retired_pack_count != 0
                    || absence_count != 0
                    || verified_target_locations == 0
                    || finished_at.is_some()
                {
                    return Err(StoreError::CorruptMetadata(
                        "repacking purge cleanup state is inconsistent",
                    ));
                }
            }
            PurgeJournalState::Cleaning => {
                if total_file_count != expected_file_count
                    || absence_count != event.object_count
                    || verified_target_locations != 0
                    || finished_at.is_some()
                {
                    return Err(StoreError::CorruptMetadata(
                        "cleaning purge state is inconsistent",
                    ));
                }
            }
            PurgeJournalState::Succeeded => {
                if total_file_count != expected_file_count
                    || progress.pending_file_count != 0
                    || progress.unlinking_file_count != 0
                    || progress.replacement_ready_pack_count != 0
                    || progress.pending_pack_count != 0
                    || progress.retired_pack_count != event.affected_pack_count
                    || absence_count != event.object_count
                    || verified_target_locations != 0
                    || finished_at.is_none()
                {
                    return Err(StoreError::CorruptMetadata(
                        "succeeded purge cleanup state is inconsistent",
                    ));
                }
            }
            PurgeJournalState::Failed => {
                return Err(StoreError::CorruptMetadata(
                    "failed purge cleanup cannot authorize absence",
                ));
            }
        }

        let (recorded_retired, recorded_replacement, directories_synced_at, completed_at) =
            accounting.ok_or(StoreError::CorruptMetadata(
                "purge cleanup accounting is missing",
            ))?;
        if progress.state == PurgeJournalState::Succeeded {
            if directories_synced_at.is_none()
                || completed_at.is_none()
                || Some(recorded_retired) != i64::try_from(progress.retired_file_bytes).ok()
                || Some(recorded_replacement) != i64::try_from(progress.replacement_file_bytes).ok()
            {
                return Err(StoreError::CorruptMetadata(
                    "completed purge accounting disagrees with cleanup work",
                ));
            }
        } else if directories_synced_at.is_some()
            || completed_at.is_some()
            || recorded_retired != 0
            || recorded_replacement != 0
        {
            return Err(StoreError::CorruptMetadata(
                "unfinished purge has finalized cleanup accounting",
            ));
        }
        self.verify_cleanup_file_rows(purge_id, progress.state, expected_file_count)?;
        self.verify_cleanup_pack_rows(purge_id, progress.state)?;
        Ok(progress)
    }

    /// Returns the exact purge-backed absence for one logical object, if present.
    ///
    /// Callers making integrity claims must additionally validate the journal's typed
    /// manifest event and cleanup state; this lookup only proves exact table
    /// membership through the schema's composite foreign key.
    pub fn purge_authorized_absence(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<PurgeAuthorizedAbsence>, StoreError> {
        type Row = (i64, String, i64, i64, Option<i64>);
        let row: Option<Row> = self
            .connection
            .query_row(
                "SELECT absence.purge_id, selected.object_kind,
                        selected.uncompressed_length, absence.absent_at,
                        absence.superseded_at
                 FROM purge_authorized_absences AS absence
                 JOIN purge_objects AS selected
                   ON selected.purge_id = absence.purge_id
                  AND selected.object_id = absence.object_id
                 WHERE absence.object_id = ?1 AND absence.superseded_at IS NULL",
                [object_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(purge_id, kind, length, absent_at, superseded_at)| {
            Ok(PurgeAuthorizedAbsence {
                purge_id: sql_u64(purge_id, "invalid authorized-absence purge ID")?,
                object: StoredObject {
                    id: object_id,
                    kind: ObjectKind::from_database(&kind)?,
                    uncompressed_length: sql_u64(
                        length,
                        "invalid authorized-absence object length",
                    )?,
                },
                absent_at: sql_u64(absent_at, "invalid authorized-absence time")?,
                superseded_at: superseded_at
                    .map(|value| sql_u64(value, "invalid absence supersession time"))
                    .transpose()?,
            })
        })
        .transpose()
    }

    /// Returns one exact historical absence record for a purge/object pair.
    pub fn purge_authorized_absence_for_purge(
        &self,
        purge_id: u64,
        object_id: ObjectId,
    ) -> Result<Option<PurgeAuthorizedAbsence>, StoreError> {
        type Row = (String, i64, i64, Option<i64>);
        let row: Option<Row> = self
            .connection
            .query_row(
                "SELECT selected.object_kind, selected.uncompressed_length,
                        absence.absent_at, absence.superseded_at
                 FROM purge_authorized_absences AS absence
                 JOIN purge_objects AS selected
                   ON selected.purge_id = absence.purge_id
                  AND selected.object_id = absence.object_id
                 WHERE absence.purge_id = ?1 AND absence.object_id = ?2",
                params![to_sql_integer(purge_id)?, object_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        row.map(|(kind, length, absent_at, superseded_at)| {
            Ok(PurgeAuthorizedAbsence {
                purge_id,
                object: StoredObject {
                    id: object_id,
                    kind: ObjectKind::from_database(&kind)?,
                    uncompressed_length: sql_u64(
                        length,
                        "invalid authorized-absence object length",
                    )?,
                },
                absent_at: sql_u64(absent_at, "invalid authorized-absence time")?,
                superseded_at: superseded_at
                    .map(|value| sql_u64(value, "invalid absence supersession time"))
                    .transpose()?,
            })
        })
        .transpose()
    }

    /// Returns a bounded oldest-first page of unfinished cleanup journals.
    pub fn unfinished_purge_cleanups(
        &self,
        after_purge_id: Option<u64>,
        limit: u32,
    ) -> Result<Vec<PurgeCleanupProgress>, StoreError> {
        if !(1..=MAX_UNFINISHED_PURGE_PAGE_SIZE).contains(&limit) {
            return Err(StoreError::InvalidConfig(
                "unfinished purge page size must be between 1 and 1,000",
            ));
        }
        let after = to_sql_integer(after_purge_id.unwrap_or(0))?;
        let ids = {
            let mut statement = self.connection.prepare(
                "SELECT purge_id FROM purge_operations
                 WHERE purge_id > ?1
                   AND state IN ('authorized', 'repacking', 'cleaning')
                 ORDER BY purge_id LIMIT ?2",
            )?;
            statement
                .query_map(params![after, i64::from(limit)], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        ids.into_iter()
            .map(|id| self.purge_cleanup_progress(sql_u64(id, "invalid unfinished purge ID")?))
            .collect()
    }

    /// Returns durable progress without changing the journal or filesystem.
    pub fn purge_cleanup_progress(
        &self,
        purge_id: u64,
    ) -> Result<PurgeCleanupProgress, StoreError> {
        let snapshot = self.purge_verification_snapshot(purge_id)?;
        let raw_purge_id = to_sql_integer(purge_id)?;
        let (pending_packs, ready_packs, retired_packs): (i64, i64, i64) =
            self.connection.query_row(
                "SELECT
                    COALESCE(SUM(state = 'pending'), 0),
                    COALESCE(SUM(state = 'replacement-ready'), 0),
                    COALESCE(SUM(state = 'retired'), 0)
                 FROM purge_pack_work WHERE purge_id = ?1",
                [raw_purge_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let (pending_files, unlinking_files, retired_files, retired_bytes): (i64, i64, i64, i64) =
            self.connection.query_row(
                "SELECT
                COALESCE(SUM(state = 'pending'), 0),
                COALESCE(SUM(state = 'unlinking'), 0),
                COALESCE(SUM(state = 'retired'), 0),
                COALESCE(SUM(CASE WHEN state = 'retired'
                                  THEN observed_file_bytes ELSE 0 END), 0)
             FROM purge_file_work WHERE purge_id = ?1",
                [raw_purge_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        let replacement_bytes: i64 = self.connection.query_row(
            "SELECT COALESCE(SUM(pack_bytes + index_bytes), 0)
             FROM purge_replacement_metrics WHERE purge_id = ?1",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        let retired_file_bytes = sql_u64(retired_bytes, "invalid retired purge byte count")?;
        let replacement_file_bytes =
            sql_u64(replacement_bytes, "invalid replacement purge byte count")?;
        let net = i128::from(retired_file_bytes) - i128::from(replacement_file_bytes);
        let net_reclaimed_file_bytes = i64::try_from(net)
            .map_err(|_| StoreError::CorruptMetadata("purge net byte count exceeds bounds"))?;
        Ok(PurgeCleanupProgress {
            purge_id,
            state: snapshot.state,
            manifest_installed: self.installed_purge_event(purge_id)?.is_some(),
            pending_pack_count: sql_u64(pending_packs, "invalid pending purge pack count")?,
            replacement_ready_pack_count: sql_u64(
                ready_packs,
                "invalid replacement-ready purge pack count",
            )?,
            retired_pack_count: sql_u64(retired_packs, "invalid retired purge pack count")?,
            pending_file_count: sql_u64(pending_files, "invalid pending purge file count")?,
            unlinking_file_count: sql_u64(unlinking_files, "invalid unlinking purge file count")?,
            retired_file_count: sql_u64(retired_files, "invalid retired purge file count")?,
            retired_file_bytes,
            replacement_file_bytes,
            net_reclaimed_file_bytes,
        })
    }

    /// Advances at most one durable cleanup checkpoint.
    ///
    /// Cancellation is safe between calls. At most one whole-pack check or mixed-pack
    /// replacement is completed per call, and no old payload becomes obsolete until
    /// every replacement has been installed, fully verified, and journaled.
    pub fn resume_purge_cleanup(
        &mut self,
        purge_id: u64,
    ) -> Result<PurgeCleanupAdvance, StoreError> {
        self.ensure_writable()?;
        let snapshot = self.purge_verification_snapshot(purge_id)?;
        let step = match snapshot.state {
            PurgeJournalState::Authorized => {
                self.append_purge_manifest(purge_id)?;
                self.validate_authenticated_purge(purge_id)?;
                self.prepare_purge_file_work(purge_id)?;
                PurgeCleanupStep::Prepared
            }
            PurgeJournalState::Repacking => {
                self.validate_authenticated_purge(purge_id)?;
                if self.mark_next_whole_pack_ready(purge_id)? {
                    PurgeCleanupStep::WholePackReady
                } else if self.replace_next_mixed_pack(purge_id)? {
                    PurgeCleanupStep::ReplacementReady
                } else {
                    self.commit_authorized_absence(purge_id)?;
                    PurgeCleanupStep::AuthorizedAbsenceCommitted
                }
            }
            PurgeJournalState::Cleaning => {
                self.validate_authenticated_purge(purge_id)?;
                if self.retire_next_file_group(purge_id)? {
                    PurgeCleanupStep::FilesRetired
                } else {
                    self.finish_purge_cleanup(purge_id)?;
                    PurgeCleanupStep::Completed
                }
            }
            PurgeJournalState::Succeeded => {
                self.validate_authenticated_purge(purge_id)?;
                PurgeCleanupStep::AlreadyComplete
            }
            PurgeJournalState::Failed => {
                return Err(StoreError::CorruptMetadata(
                    "failed purge journal cannot be resumed",
                ));
            }
        };
        Ok(PurgeCleanupAdvance {
            step,
            progress: self.purge_cleanup_progress(purge_id)?,
        })
    }

    fn installed_purge_event(&self, purge_id: u64) -> Result<Option<PurgeManifest>, StoreError> {
        let mut found = None;
        for stored in self.validated_manifest_chain()? {
            let Some(event) = stored.manifest.purge() else {
                continue;
            };
            if event.purge_id != purge_id {
                continue;
            }
            if found.replace(event.clone()).is_some() {
                return Err(StoreError::CorruptMetadata(
                    "purge journal occurs more than once in manifest chain",
                ));
            }
        }
        Ok(found)
    }

    fn validate_authenticated_purge(&self, purge_id: u64) -> Result<PurgeManifest, StoreError> {
        let snapshot = self.purge_verification_snapshot(purge_id)?;
        if snapshot.shared_object_count != 0 && snapshot.state != PurgeJournalState::Succeeded {
            return Err(StoreError::CorruptMetadata(
                "purge object inventory gained a retained reference",
            ));
        }
        let installed =
            self.installed_purge_event(purge_id)?
                .ok_or(StoreError::CorruptMetadata(
                    "purge cleanup lacks an authenticated manifest event",
                ))?;
        if installed != snapshot.expected_manifest {
            return Err(StoreError::CorruptMetadata(
                "purge manifest and cleanup journal disagree",
            ));
        }
        if snapshot.state == PurgeJournalState::Succeeded
            && self.active_absence_shared_reference_count(purge_id, &installed)? != 0
        {
            return Err(StoreError::CorruptMetadata(
                "active purge absence gained a retained reference",
            ));
        }
        let collection: Option<(String, i64, String, Option<i64>)> = self
            .connection
            .query_row(
                "SELECT name, generation, status, tombstoned_at
                 FROM collections WHERE collection_id = ?1",
                [to_sql_integer(installed.collection_id.get())?],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let (name, generation, status, tombstoned_at) = collection.ok_or(
            StoreError::CorruptMetadata("purge collection disappeared during cleanup"),
        )?;
        if name != installed.collection_name
            || generation != to_sql_integer(installed.collection_generation)?
            || status != "tombstoned"
            || tombstoned_at != Some(to_sql_integer(installed.tombstoned_at)?)
        {
            return Err(StoreError::CorruptMetadata(
                "purge collection changed after authorization",
            ));
        }
        Ok(installed)
    }

    fn verify_managed_recorded_pack(&self, pack_id: &str) -> Result<RecordedPack, StoreError> {
        let pack = self.recorded_pack(pack_id)?;
        validate_managed_file_ancestors(&self.root, &pack.pack_path)?;
        validate_managed_file_ancestors(&self.root, &pack.index_path)?;
        verify_pack_files(
            &self.root.join(&pack.pack_path),
            &self.root.join(&pack.index_path),
            pack.pack_checksum,
            pack.index_checksum,
            pack.generation,
            self.config.max_object_bytes,
            pack.object_count,
        )?;
        Ok(pack)
    }

    fn active_absence_shared_reference_count(
        &self,
        purge_id: u64,
        event: &PurgeManifest,
    ) -> Result<u64, StoreError> {
        let manifests = self.validated_manifest_chain()?;
        let (_, protected_manifest_objects) =
            purge_manifest_binding(self, &manifests, event.collection_id)?;
        let mut shared = HashSet::new();
        let mut statement = self.connection.prepare(
            "WITH target_pages AS (
                 SELECT wiki_id, page_id FROM collection_resolved_members
                 WHERE collection_id = ?2
             ), exclusive_pages AS (
                 SELECT target.wiki_id, target.page_id FROM target_pages AS target
                 WHERE NOT EXISTS (
                     SELECT 1 FROM collection_resolved_members AS other
                     WHERE other.wiki_id = target.wiki_id
                       AND other.page_id = target.page_id
                       AND other.collection_id != ?2
                 )
             )
             SELECT journal.object_id
             FROM purge_objects AS journal
             JOIN purge_authorized_absences AS absence
               ON absence.purge_id = journal.purge_id
              AND absence.object_id = journal.object_id
              AND absence.superseded_at IS NULL
             WHERE journal.purge_id = ?1
               AND ((journal.object_kind = 'wikitext' AND EXISTS (
                     SELECT 1 FROM revisions AS retained
                     WHERE retained.content_object_id = journal.object_id
                       AND NOT EXISTS (
                           SELECT 1 FROM exclusive_pages AS target
                           WHERE target.wiki_id = retained.wiki_id
                             AND target.page_id = retained.page_id
                       )
                 )) OR (journal.object_kind = 'media' AND (
                     EXISTS (
                         SELECT 1 FROM page_media AS placement
                         JOIN revisions AS revision
                           ON revision.wiki_id = placement.wiki_id
                          AND revision.revision_id = placement.revision_id
                         WHERE placement.content_object_id = journal.object_id
                           AND NOT EXISTS (
                               SELECT 1 FROM exclusive_pages AS target
                               WHERE target.wiki_id = revision.wiki_id
                                 AND target.page_id = revision.page_id
                           )
                     ) OR EXISTS (
                         SELECT 1 FROM media AS catalog
                         WHERE catalog.content_object_id = journal.object_id
                           AND NOT EXISTS (
                               SELECT 1 FROM page_media AS placement
                               JOIN revisions AS revision
                                 ON revision.wiki_id = placement.wiki_id
                                AND revision.revision_id = placement.revision_id
                               JOIN exclusive_pages AS target
                                 ON target.wiki_id = revision.wiki_id
                                AND target.page_id = revision.page_id
                               WHERE placement.wiki_id = catalog.wiki_id
                                 AND placement.source_media_id = catalog.source_media_id
                                 AND placement.source_sha1 = catalog.source_sha1
                                 AND placement.content_object_id = catalog.content_object_id
                           )
                     )
                 )))
             ORDER BY journal.object_id LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                to_sql_integer(purge_id)?,
                to_sql_integer(event.collection_id.get())?,
                i64::from(MAX_PURGE_OBJECTS) + 1
            ],
            |row| row.get::<_, String>(0),
        )?;
        for row in rows {
            let id: ObjectId = row?
                .parse()
                .map_err(|_| StoreError::CorruptMetadata("invalid purge object identity"))?;
            shared.insert(id);
            if shared.len() > MAX_PURGE_OBJECTS as usize {
                return Err(StoreError::PurgeLimitExceeded);
            }
        }
        if !protected_manifest_objects.is_empty() {
            let mut statement = self.connection.prepare(
                "SELECT journal.object_id
                 FROM purge_objects AS journal
                 JOIN purge_authorized_absences AS absence
                   ON absence.purge_id = journal.purge_id
                  AND absence.object_id = journal.object_id
                  AND absence.superseded_at IS NULL
                 WHERE journal.purge_id = ?1
                 ORDER BY journal.object_id LIMIT ?2",
            )?;
            let rows = statement.query_map(
                params![to_sql_integer(purge_id)?, i64::from(MAX_PURGE_OBJECTS) + 1],
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                let id: ObjectId = row?
                    .parse()
                    .map_err(|_| StoreError::CorruptMetadata("invalid purge object identity"))?;
                if protected_manifest_objects.contains(&id) {
                    shared.insert(id);
                }
                if shared.len() > MAX_PURGE_OBJECTS as usize {
                    return Err(StoreError::PurgeLimitExceeded);
                }
            }
        }
        u64::try_from(shared.len()).map_err(|_| StoreError::PurgeLimitExceeded)
    }

    fn verify_cleanup_file_rows(
        &self,
        purge_id: u64,
        phase: PurgeJournalState,
        expected_count: u64,
    ) -> Result<(), StoreError> {
        type Row = (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
            Option<i64>,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
        );
        let limit = expected_count
            .checked_add(1)
            .ok_or(StoreError::PurgeLimitExceeded)?;
        let rows: Vec<Row> = {
            let mut statement = self.connection.prepare(
                "SELECT work.file_kind, work.relative_path, work.object_id,
                        work.old_pack_id, work.expected_checksum,
                        work.expected_file_bytes, work.observed_file_bytes, work.state,
                        pack.pack_path, pack.index_path,
                        pack.pack_checksum, pack.index_checksum,
                        absence.superseded_at
                 FROM purge_file_work AS work
                 LEFT JOIN packs AS pack ON pack.pack_id = work.old_pack_id
                 LEFT JOIN purge_authorized_absences AS absence
                   ON absence.purge_id = work.purge_id
                  AND absence.object_id = work.object_id
                 WHERE work.purge_id = ?1
                 ORDER BY work.file_kind, work.relative_path LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![to_sql_integer(purge_id)?, to_sql_integer(limit)?],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                            row.get(9)?,
                            row.get(10)?,
                            row.get(11)?,
                            row.get(12)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        if rows.len() as u64 != expected_count {
            return Err(StoreError::CorruptMetadata(
                "purge cleanup file inventory count disagrees",
            ));
        }
        for (
            kind,
            raw_path,
            raw_object_id,
            old_pack_id,
            expected_checksum,
            expected_bytes,
            observed_bytes,
            state,
            pack_path,
            index_path,
            pack_checksum,
            index_checksum,
            superseded_at,
        ) in rows
        {
            let path = match kind.as_str() {
                "loose" => {
                    let object_id: ObjectId = raw_object_id
                        .as_deref()
                        .ok_or(StoreError::CorruptMetadata(
                            "cleanup loose work lacks object identity",
                        ))?
                        .parse()
                        .map_err(|_| {
                            StoreError::CorruptMetadata("invalid cleanup loose object identity")
                        })?;
                    let path = loose_database_path(&raw_path)?;
                    if path != loose_relative_path(object_id)
                        || old_pack_id.is_some()
                        || expected_checksum.is_some()
                    {
                        return Err(StoreError::CorruptMetadata(
                            "cleanup loose work binding disagrees",
                        ));
                    }
                    path
                }
                "pack" => {
                    let path = pack_database_path(&raw_path, ".pack")?;
                    if raw_object_id.is_some()
                        || old_pack_id.is_none()
                        || pack_path.as_deref() != Some(raw_path.as_str())
                        || expected_checksum != pack_checksum
                    {
                        return Err(StoreError::CorruptMetadata(
                            "cleanup pack work binding disagrees",
                        ));
                    }
                    path
                }
                "index" => {
                    let path = pack_database_path(&raw_path, ".idx")?;
                    if raw_object_id.is_some()
                        || old_pack_id.is_none()
                        || index_path.as_deref() != Some(raw_path.as_str())
                        || expected_checksum != index_checksum
                    {
                        return Err(StoreError::CorruptMetadata(
                            "cleanup index work binding disagrees",
                        ));
                    }
                    path
                }
                _ => {
                    return Err(StoreError::CorruptMetadata(
                        "unknown purge cleanup file kind",
                    ));
                }
            };
            validate_managed_file_ancestors(&self.root, &path)?;
            let absolute = self.root.join(path);
            let present = regular_file_exists(&absolute)?;
            let expected_bytes = sql_u64(expected_bytes, "invalid cleanup file byte count")?;
            let observed_bytes = observed_bytes
                .map(|value| sql_u64(value, "invalid observed cleanup file byte count"))
                .transpose()?;
            if (state == "pending" && observed_bytes.is_some())
                || (state != "pending" && observed_bytes != Some(expected_bytes))
            {
                return Err(StoreError::CorruptMetadata(
                    "observed cleanup file bytes disagree with prepared inventory",
                ));
            }
            if phase == PurgeJournalState::Succeeded
                && state == "retired"
                && present
                && kind == "loose"
                && superseded_at.is_some()
            {
                let object_id: ObjectId = raw_object_id
                    .as_deref()
                    .ok_or(StoreError::CorruptMetadata(
                        "superseded loose work lacks object identity",
                    ))?
                    .parse()
                    .map_err(|_| {
                        StoreError::CorruptMetadata("invalid superseded object identity")
                    })?;
                let verified: bool = self.connection.query_row(
                    "SELECT EXISTS (
                        SELECT 1 FROM object_locations
                        WHERE object_id = ?1 AND storage_kind = 'loose'
                          AND relative_path = ?2 AND verification_state = 'verified'
                     )",
                    params![object_id.to_string(), raw_path],
                    |row| row.get(0),
                )?;
                if !verified {
                    return Err(StoreError::CorruptMetadata(
                        "superseded purge path lacks a verified rehydrated location",
                    ));
                }
                self.read_object(object_id)?;
                continue;
            }
            match (phase, state.as_str(), present) {
                (PurgeJournalState::Repacking, "pending", true)
                | (PurgeJournalState::Cleaning, "pending" | "unlinking", true) => {
                    if checked_regular_file_length(&absolute)? != expected_bytes {
                        return Err(StoreError::CorruptMetadata(
                            "purge cleanup file length disagrees",
                        ));
                    }
                }
                (PurgeJournalState::Cleaning, "unlinking" | "retired", false)
                | (PurgeJournalState::Succeeded, "retired", false) => {}
                _ => {
                    return Err(StoreError::CorruptMetadata(
                        "purge cleanup file state and presence disagree",
                    ));
                }
            }
        }
        Ok(())
    }

    fn verify_cleanup_pack_rows(
        &self,
        purge_id: u64,
        phase: PurgeJournalState,
    ) -> Result<(), StoreError> {
        let raw_purge_id = to_sql_integer(purge_id)?;
        let invalid: i64 = match phase {
            PurgeJournalState::Repacking => self.connection.query_row(
                "SELECT COUNT(*) FROM purge_pack_work AS work
                 JOIN packs AS pack ON pack.pack_id = work.old_pack_id
                 WHERE work.purge_id = ?1
                   AND (work.state NOT IN ('pending', 'replacement-ready')
                        OR pack.state != 'verified')",
                [raw_purge_id],
                |row| row.get(0),
            )?,
            PurgeJournalState::Cleaning | PurgeJournalState::Succeeded => {
                self.connection.query_row(
                    "SELECT COUNT(*) FROM purge_pack_work AS work
                     JOIN packs AS pack ON pack.pack_id = work.old_pack_id
                     WHERE work.purge_id = ?1
                       AND (work.state NOT IN ('replacement-ready', 'retired')
                            OR pack.state != 'obsolete')",
                    [raw_purge_id],
                    |row| row.get(0),
                )?
            }
            _ => 0,
        };
        if invalid != 0 {
            return Err(StoreError::CorruptMetadata(
                "purge cleanup pack phase is inconsistent",
            ));
        }
        let invalid_metrics: i64 = match phase {
            PurgeJournalState::Repacking => self.connection.query_row(
                "SELECT COUNT(*) FROM purge_pack_work AS work
                 LEFT JOIN purge_replacement_metrics AS metric
                   ON metric.purge_id = work.purge_id
                  AND metric.old_pack_id = work.old_pack_id
                 WHERE work.purge_id = ?1
                   AND ((work.retained_object_count = 0
                         AND (work.replacement_pack_id IS NOT NULL
                              OR metric.old_pack_id IS NOT NULL))
                        OR (work.state = 'replacement-ready'
                            AND work.retained_object_count > 0
                            AND (work.replacement_pack_id IS NULL
                                 OR metric.old_pack_id IS NULL
                                 OR metric.replacement_pack_id != work.replacement_pack_id)))",
                [raw_purge_id],
                |row| row.get(0),
            )?,
            PurgeJournalState::Cleaning | PurgeJournalState::Succeeded => {
                self.connection.query_row(
                    "SELECT COUNT(*) FROM purge_pack_work AS work
                     LEFT JOIN purge_replacement_metrics AS metric
                       ON metric.purge_id = work.purge_id
                      AND metric.old_pack_id = work.old_pack_id
                     WHERE work.purge_id = ?1
                       AND ((work.retained_object_count = 0
                             AND (work.replacement_pack_id IS NOT NULL
                                  OR metric.old_pack_id IS NOT NULL))
                            OR (work.retained_object_count > 0
                                AND (work.replacement_pack_id IS NULL
                                     OR metric.old_pack_id IS NULL
                                     OR metric.replacement_pack_id != work.replacement_pack_id)))",
                    [raw_purge_id],
                    |row| row.get(0),
                )?
            }
            _ => 0,
        };
        if invalid_metrics != 0 {
            return Err(StoreError::CorruptMetadata(
                "purge replacement metrics disagree with pack work",
            ));
        }
        Ok(())
    }

    fn prepare_purge_file_work(&mut self, purge_id: u64) -> Result<(), StoreError> {
        let event = self.validate_authenticated_purge(purge_id)?;
        self.validate_authorized_preview(&event)?;
        let raw_purge_id = to_sql_integer(purge_id)?;
        let existing: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM purge_file_work WHERE purge_id = ?1",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        if existing != 0 {
            return Err(StoreError::CorruptMetadata(
                "authorized purge already has physical cleanup work",
            ));
        }

        type LooseRow = (i64, String, String, String, i64, i64);
        let loose_rows: Vec<LooseRow> = {
            let mut statement = self.connection.prepare(
                "SELECT location.location_id, location.object_id,
                        object.object_kind, location.relative_path,
                        location.compressed_length, object.uncompressed_length
                 FROM object_locations AS location
                 JOIN purge_objects AS selected
                   ON selected.purge_id = ?1
                  AND selected.object_id = location.object_id
                 JOIN content_objects AS object USING (object_id)
                 WHERE location.storage_kind = 'loose'
                   AND location.encoding = 'zstd'
                   AND location.verification_state = 'verified'
                   AND object.verification_state = 'verified'
                 ORDER BY location.location_id LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![raw_purge_id, i64::from(MAX_PURGE_LOCATIONS) + 1],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        if loose_rows.len() > MAX_PURGE_LOCATIONS as usize {
            return Err(StoreError::PurgeLocationLimitExceeded);
        }
        let mut prepared_loose = Vec::with_capacity(loose_rows.len());
        for (location_id, raw_id, raw_kind, raw_path, compressed, uncompressed) in loose_rows {
            let id: ObjectId = raw_id
                .parse()
                .map_err(|_| StoreError::CorruptMetadata("invalid purge loose object ID"))?;
            let kind = ObjectKind::from_database(&raw_kind)?;
            let expected_length = sql_u64(uncompressed, "invalid purge loose object length")?;
            let path = loose_database_path(&raw_path)?;
            if path != loose_relative_path(id) {
                return Err(StoreError::CorruptMetadata(
                    "purge loose path disagrees with content identity",
                ));
            }
            validate_managed_file_ancestors(&self.root, &path)?;
            self.read_loose_location(id, kind, expected_length, &raw_path)?;
            let file_bytes = checked_regular_file_length(&self.root.join(&path))?;
            if file_bytes != sql_u64(compressed, "invalid purge loose file length")? {
                return Err(StoreError::CorruptMetadata(
                    "purge loose file length disagrees with location",
                ));
            }
            prepared_loose.push((location_id, raw_id, raw_path, file_bytes));
        }
        let prepared_loose_object_count = prepared_loose
            .iter()
            .map(|(_, object_id, _, _)| object_id)
            .collect::<HashSet<_>>()
            .len() as u64;
        if prepared_loose_object_count != event.loose_object_count {
            return Err(StoreError::CorruptMetadata(
                "purge loose inventory disagrees with manifest",
            ));
        }

        let pack_ids: Vec<String> = {
            let mut statement = self.connection.prepare(
                "SELECT old_pack_id FROM purge_pack_work
                 WHERE purge_id = ?1 ORDER BY old_pack_id LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![raw_purge_id, i64::from(MAX_PURGE_AFFECTED_PACKS) + 1],
                    |row| row.get(0),
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        if pack_ids.len() > MAX_PURGE_AFFECTED_PACKS as usize {
            return Err(StoreError::PurgePackLimitExceeded);
        }
        let mut prepared_packs = Vec::with_capacity(pack_ids.len());
        for pack_id in pack_ids {
            let recorded = self.verify_managed_recorded_pack(&pack_id)?;
            let pack_bytes = checked_regular_file_length(&self.root.join(&recorded.pack_path))?;
            let index_bytes = checked_regular_file_length(&self.root.join(&recorded.index_path))?;
            prepared_packs.push((
                pack_id,
                path_to_database(&recorded.pack_path)?,
                path_to_database(&recorded.index_path)?,
                format!(
                    "b3:{}",
                    blake3::Hash::from_bytes(recorded.pack_checksum).to_hex()
                ),
                format!(
                    "b3:{}",
                    blake3::Hash::from_bytes(recorded.index_checksum).to_hex()
                ),
                pack_bytes,
                index_bytes,
            ));
        }
        if prepared_packs.len() as u64 != event.affected_pack_count {
            return Err(StoreError::CorruptMetadata(
                "purge physical pack inventory disagrees with manifest",
            ));
        }

        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: String = transaction.query_row(
            "SELECT state FROM purge_operations WHERE purge_id = ?1",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        if state != "authorized" {
            return Err(StoreError::PurgeJournalNotAuthorized(purge_id));
        }
        for (location_id, object_id, path, bytes) in prepared_loose {
            transaction.execute(
                "INSERT INTO purge_file_work (
                    purge_id, file_kind, relative_path, location_id, object_id,
                    old_pack_id, expected_checksum, expected_file_bytes,
                    state, observed_file_bytes, prepared_at,
                    unlink_started_at, retired_at
                 ) VALUES (?1, 'loose', ?2, ?3, ?4, NULL, NULL, ?5,
                           'pending', NULL, ?6, NULL, NULL)",
                params![
                    raw_purge_id,
                    path,
                    location_id,
                    object_id,
                    to_sql_integer(bytes)?,
                    now
                ],
            )?;
        }
        for (
            pack_id,
            pack_path,
            index_path,
            pack_checksum,
            index_checksum,
            pack_bytes,
            index_bytes,
        ) in prepared_packs
        {
            transaction.execute(
                "INSERT INTO purge_file_work (
                    purge_id, file_kind, relative_path, location_id, object_id,
                    old_pack_id, expected_checksum, expected_file_bytes,
                    state, observed_file_bytes, prepared_at,
                    unlink_started_at, retired_at
                 ) VALUES (?1, 'pack', ?2, NULL, NULL, ?3, ?4, ?5,
                           'pending', NULL, ?6, NULL, NULL)",
                params![
                    raw_purge_id,
                    pack_path,
                    pack_id,
                    pack_checksum,
                    to_sql_integer(pack_bytes)?,
                    now
                ],
            )?;
            transaction.execute(
                "INSERT INTO purge_file_work (
                    purge_id, file_kind, relative_path, location_id, object_id,
                    old_pack_id, expected_checksum, expected_file_bytes,
                    state, observed_file_bytes, prepared_at,
                    unlink_started_at, retired_at
                 ) VALUES (?1, 'index', ?2, NULL, NULL, ?3, ?4, ?5,
                           'pending', NULL, ?6, NULL, NULL)",
                params![
                    raw_purge_id,
                    index_path,
                    pack_id,
                    index_checksum,
                    to_sql_integer(index_bytes)?,
                    now
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO purge_cleanup_accounting (
                purge_id, retired_file_bytes, replacement_file_bytes,
                directories_synced_at, completed_at
             ) VALUES (?1, 0, 0, NULL, NULL)",
            [raw_purge_id],
        )?;
        let changed = transaction.execute(
            "UPDATE purge_operations
             SET state = 'repacking', updated_at = ?2
             WHERE purge_id = ?1 AND state = 'authorized'",
            params![raw_purge_id, now],
        )?;
        if changed != 1 {
            return Err(StoreError::PurgeJournalNotAuthorized(purge_id));
        }
        transaction.commit()?;
        Ok(())
    }

    fn mark_next_whole_pack_ready(&mut self, purge_id: u64) -> Result<bool, StoreError> {
        let raw_purge_id = to_sql_integer(purge_id)?;
        let pack_id: Option<String> = self
            .connection
            .query_row(
                "SELECT old_pack_id FROM purge_pack_work
                 WHERE purge_id = ?1 AND state = 'pending'
                   AND retained_object_count = 0
                 ORDER BY old_pack_id LIMIT 1",
                [raw_purge_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(pack_id) = pack_id else {
            return Ok(false);
        };
        self.verify_managed_recorded_pack(&pack_id)?;
        let changed = self.connection.execute(
            "UPDATE purge_pack_work
             SET state = 'replacement-ready'
             WHERE purge_id = ?1 AND old_pack_id = ?2
               AND retained_object_count = 0
               AND replacement_pack_id IS NULL AND state = 'pending'",
            params![raw_purge_id, pack_id],
        )?;
        if changed != 1 {
            return Err(StoreError::CorruptMetadata(
                "whole-pack purge work changed concurrently",
            ));
        }
        Ok(true)
    }

    fn replace_next_mixed_pack(&mut self, purge_id: u64) -> Result<bool, StoreError> {
        type Work = (String, i64, i64);
        let work: Option<Work> = self
            .connection
            .query_row(
                "SELECT old_pack_id, purged_object_count, retained_object_count
                 FROM purge_pack_work
                 WHERE purge_id = ?1 AND state = 'pending'
                   AND retained_object_count > 0
                 ORDER BY old_pack_id LIMIT 1",
                [to_sql_integer(purge_id)?],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((old_pack_id, raw_purged_count, raw_retained_count)) = work else {
            return Ok(false);
        };
        let purged_count = sql_u64(raw_purged_count, "invalid purged pack object count")?;
        let retained_count = sql_u64(raw_retained_count, "invalid retained pack object count")?;
        if retained_count == 0 || retained_count > u64::from(self.config.max_pack_objects) {
            return Err(StoreError::PackLimitExceeded);
        }

        // This reconstructs and hash-verifies every old entry, including delta bases
        // that are themselves selected for purge, before any replacement is built.
        let recorded = self.verify_managed_recorded_pack(&old_pack_id)?;
        if recorded.object_count
            != purged_count
                .checked_add(retained_count)
                .ok_or(StoreError::CorruptMetadata(
                    "mixed purge pack object count overflow",
                ))?
        {
            return Err(StoreError::CorruptMetadata(
                "mixed purge pack counts disagree with recorded pack",
            ));
        }

        type Candidate = (
            String,
            i64,
            String,
            i64,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        );
        let candidates: Vec<Candidate> = {
            let mut statement = self.connection.prepare(
                "SELECT location.object_id, location.pack_offset,
                        object.object_kind, object.uncompressed_length,
                        selected.object_id,
                        affinity.wiki_id, affinity.page_id, affinity.revision_id
                 FROM object_locations AS location
                 JOIN content_objects AS object USING (object_id)
                 LEFT JOIN purge_objects AS selected
                   ON selected.purge_id = ?1
                  AND selected.object_id = location.object_id
                 LEFT JOIN revisions AS affinity ON affinity.rowid = (
                     SELECT revision.rowid
                     FROM revisions AS revision
                     WHERE revision.content_object_id = location.object_id
                     ORDER BY revision.wiki_id, revision.page_id, revision.revision_id
                     LIMIT 1
                 )
                 WHERE location.pack_id = ?2
                   AND location.verification_state = 'verified'
                 ORDER BY location.pack_offset LIMIT ?3",
            )?;
            statement
                .query_map(
                    params![
                        to_sql_integer(purge_id)?,
                        old_pack_id,
                        i64::from(MAX_SUPPORTED_PACK_OBJECTS) + 1
                    ],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        if candidates.len() > MAX_SUPPORTED_PACK_OBJECTS as usize {
            return Err(StoreError::PackLimitExceeded);
        }
        if candidates.len() as u64 != recorded.object_count {
            return Err(StoreError::CorruptMetadata(
                "mixed purge pack locations disagree with recorded pack",
            ));
        }

        let mut observed_purged = 0_u64;
        let mut total_input = 0_u64;
        let mut sources = Vec::with_capacity(retained_count as usize);
        for (
            raw_id,
            raw_offset,
            raw_kind,
            raw_length,
            selected,
            raw_wiki_id,
            raw_page_id,
            raw_revision_id,
        ) in candidates
        {
            if selected.is_some() {
                observed_purged = observed_purged
                    .checked_add(1)
                    .ok_or(StoreError::PackLimitExceeded)?;
                continue;
            }
            let id: ObjectId = raw_id.parse().map_err(|_| {
                StoreError::CorruptMetadata("invalid mixed purge pack object identity")
            })?;
            let kind = ObjectKind::from_database(&raw_kind)?;
            let expected_length = sql_u64(raw_length, "invalid mixed pack object length")?;
            total_input = total_input
                .checked_add(expected_length)
                .ok_or(StoreError::PackLimitExceeded)?;
            if total_input > self.config.max_pack_input_bytes {
                return Err(StoreError::PackLimitExceeded);
            }
            let bytes = self.read_object(id)?;
            verify_object_bytes(id, kind, expected_length, &bytes)?;
            let (affinity, revision_order) =
                pack_affinity(raw_wiki_id, raw_page_id, raw_revision_id)?;
            sources.push(PackSource {
                id,
                kind,
                bytes,
                affinity,
                revision_order,
                stable_order: sql_u64(raw_offset, "invalid mixed pack offset")?,
            });
        }
        if observed_purged != purged_count || sources.len() as u64 != retained_count {
            return Err(StoreError::CorruptMetadata(
                "mixed purge retained subset disagrees with journal",
            ));
        }
        sources.sort_by_key(PackSource::sort_key);
        self.activate_pack_sources_for_purge(
            &sources,
            Some((purge_id, &old_pack_id, retained_count)),
        )?;
        Ok(true)
    }

    fn commit_authorized_absence(&mut self, purge_id: u64) -> Result<(), StoreError> {
        let event = self.validate_authenticated_purge(purge_id)?;
        self.validate_authorized_preview(&event)?;
        let manifests = self.validated_manifest_chain()?;
        let (_, protected_manifest_objects) =
            purge_manifest_binding(self, &manifests, event.collection_id)?;
        let raw_purge_id = to_sql_integer(purge_id)?;
        self.validate_replacements(purge_id)?;
        self.validate_physical_work_snapshot(purge_id)?;
        let raw_object_ids: Vec<String> = {
            let mut statement = self.connection.prepare(
                "SELECT object_id FROM purge_objects
                 WHERE purge_id = ?1 ORDER BY object_id LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![raw_purge_id, i64::from(MAX_PURGE_OBJECTS) + 1],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        if raw_object_ids.len() > MAX_PURGE_OBJECTS as usize {
            return Err(StoreError::PurgeLimitExceeded);
        }
        let object_ids = raw_object_ids
            .into_iter()
            .map(|id| {
                id.parse::<ObjectId>()
                    .map_err(|_| StoreError::CorruptMetadata("invalid purge object identity"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for id in &object_ids {
            self.read_object(*id)?;
        }

        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let locked_snapshot = purge_verification_snapshot(&transaction, purge_id)?;
        if locked_snapshot.expected_manifest != event
            || purge_shared_reference_count(
                &transaction,
                purge_id,
                event.collection_id,
                &protected_manifest_objects,
            )? != 0
        {
            return Err(StoreError::CorruptMetadata(
                "purge authorization changed before logical cleanup",
            ));
        }
        let locked_preview = compute_purge_preview(
            &transaction,
            event.collection_id,
            event.pre_purge_head_sequence.zip(event.pre_purge_head_id),
            &protected_manifest_objects,
            Some(event.purge_id),
        )?
        .0;
        if locked_preview != purge_preview_from_manifest(&event) {
            return Err(StoreError::StalePurgePreview(event.collection_id));
        }
        let state: String = transaction.query_row(
            "SELECT state FROM purge_operations WHERE purge_id = ?1",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        if state != "repacking" {
            return Err(StoreError::CorruptMetadata(
                "purge journal left repacking before logical cleanup",
            ));
        }
        let pending: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM purge_pack_work
             WHERE purge_id = ?1 AND state != 'replacement-ready'",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        if pending != 0 {
            return Err(StoreError::CorruptMetadata(
                "purge pack replacement is incomplete",
            ));
        }

        let search_ids: Vec<i64> = {
            let mut statement = transaction.prepare(
                "SELECT document.search_id
                 FROM search_documents AS document
                 JOIN revisions AS revision
                   ON revision.wiki_id = document.wiki_id
                  AND revision.revision_id = document.revision_id
                 JOIN purge_objects AS selected
                   ON selected.purge_id = ?1
                  AND selected.object_id = revision.content_object_id
                 ORDER BY document.search_id LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![raw_purge_id, i64::from(MAX_PURGE_LOCATIONS) + 1],
                    |row| row.get(0),
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        if search_ids.len() > MAX_PURGE_LOCATIONS as usize {
            return Err(StoreError::PurgeLocationLimitExceeded);
        }
        for search_id in search_ids {
            transaction.execute("DELETE FROM search_fts WHERE rowid = ?1", [search_id])?;
            transaction.execute(
                "DELETE FROM search_documents WHERE search_id = ?1",
                [search_id],
            )?;
        }
        let absences = transaction.execute(
            "INSERT INTO purge_authorized_absences (purge_id, object_id, absent_at)
             SELECT purge_id, object_id, ?2 FROM purge_objects WHERE purge_id = ?1",
            params![raw_purge_id, now],
        )?;
        if absences as u64 != event.object_count {
            return Err(StoreError::CorruptMetadata(
                "authorized absence count disagrees with purge manifest",
            ));
        }
        transaction.execute(
            "UPDATE object_locations SET verification_state = 'obsolete'
             WHERE verification_state = 'verified'
               AND object_id IN (
                   SELECT object_id FROM purge_objects WHERE purge_id = ?1
               )",
            [raw_purge_id],
        )?;
        transaction.execute(
            "UPDATE object_locations SET verification_state = 'obsolete'
             WHERE verification_state = 'verified'
               AND pack_id IN (
                   SELECT old_pack_id FROM purge_pack_work WHERE purge_id = ?1
               )",
            [raw_purge_id],
        )?;
        transaction.execute(
            "UPDATE packs SET state = 'obsolete'
             WHERE state = 'verified'
               AND pack_id IN (
                   SELECT old_pack_id FROM purge_pack_work WHERE purge_id = ?1
               )",
            [raw_purge_id],
        )?;
        let changed = transaction.execute(
            "UPDATE purge_operations
             SET state = 'cleaning', updated_at = ?2
             WHERE purge_id = ?1 AND state = 'repacking'",
            params![raw_purge_id, now],
        )?;
        if changed != 1 {
            return Err(StoreError::CorruptMetadata(
                "purge journal did not enter cleaning",
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    fn validate_authorized_preview(&self, event: &PurgeManifest) -> Result<(), StoreError> {
        let manifests = self.validated_manifest_chain()?;
        let (_, protected_manifest_objects) =
            purge_manifest_binding(self, &manifests, event.collection_id)?;
        let current = compute_purge_preview(
            &self.connection,
            event.collection_id,
            event.pre_purge_head_sequence.zip(event.pre_purge_head_id),
            &protected_manifest_objects,
            Some(event.purge_id),
        )?
        .0;
        if current != purge_preview_from_manifest(event) {
            return Err(StoreError::StalePurgePreview(event.collection_id));
        }
        Ok(())
    }

    fn validate_replacements(&self, purge_id: u64) -> Result<(), StoreError> {
        type RawWork = (String, i64, Option<String>, String);
        let raw_work: Vec<RawWork> = {
            let mut statement = self.connection.prepare(
                "SELECT old_pack_id, retained_object_count, replacement_pack_id, state
                 FROM purge_pack_work WHERE purge_id = ?1
                 ORDER BY old_pack_id LIMIT ?2",
            )?;
            statement
                .query_map(
                    params![
                        to_sql_integer(purge_id)?,
                        i64::from(MAX_PURGE_AFFECTED_PACKS) + 1
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        if raw_work.len() > MAX_PURGE_AFFECTED_PACKS as usize {
            return Err(StoreError::PurgePackLimitExceeded);
        }
        for (old_pack_id, retained_count, replacement, state) in raw_work {
            let retained_count = sql_u64(retained_count, "invalid retained purge pack count")?;
            if state != "replacement-ready" {
                return Err(StoreError::CorruptMetadata(
                    "purge replacement is not ready",
                ));
            }
            if retained_count == 0 {
                if replacement.is_some() {
                    return Err(StoreError::CorruptMetadata(
                        "whole purge pack unexpectedly has a replacement",
                    ));
                }
                let metric_exists: bool = self.connection.query_row(
                    "SELECT EXISTS (
                        SELECT 1 FROM purge_replacement_metrics
                        WHERE purge_id = ?1 AND old_pack_id = ?2
                     )",
                    params![to_sql_integer(purge_id)?, old_pack_id],
                    |row| row.get(0),
                )?;
                if metric_exists {
                    return Err(StoreError::CorruptMetadata(
                        "whole purge pack unexpectedly has replacement metrics",
                    ));
                }
                continue;
            }
            let replacement = replacement.ok_or(StoreError::CorruptMetadata(
                "mixed purge pack lacks a replacement",
            ))?;
            if replacement == old_pack_id {
                return Err(StoreError::CorruptMetadata(
                    "purge replacement aliases the retired pack",
                ));
            }
            let recorded = self.verify_managed_recorded_pack(&replacement)?;
            if recorded.object_count != retained_count {
                return Err(StoreError::CorruptMetadata(
                    "purge replacement object count disagrees",
                ));
            }
            let retained = self.pack_object_ids_excluding_purge(purge_id, &old_pack_id)?;
            let replacement_ids = self.pack_object_ids_excluding_purge(purge_id, &replacement)?;
            if retained != replacement_ids {
                return Err(StoreError::CorruptMetadata(
                    "purge replacement object inventory disagrees",
                ));
            }
            let metric: Option<(String, i64, i64)> = self
                .connection
                .query_row(
                    "SELECT replacement_pack_id, pack_bytes, index_bytes
                     FROM purge_replacement_metrics
                     WHERE purge_id = ?1 AND old_pack_id = ?2",
                    params![to_sql_integer(purge_id)?, old_pack_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let (metric_replacement, metric_pack_bytes, metric_index_bytes) = metric.ok_or(
                StoreError::CorruptMetadata("mixed purge pack lacks replacement metrics"),
            )?;
            let actual_pack_bytes =
                checked_regular_file_length(&self.root.join(&recorded.pack_path))?;
            let actual_index_bytes =
                checked_regular_file_length(&self.root.join(&recorded.index_path))?;
            if metric_replacement != replacement
                || sql_u64(metric_pack_bytes, "invalid replacement pack byte count")?
                    != actual_pack_bytes
                || sql_u64(metric_index_bytes, "invalid replacement index byte count")?
                    != actual_index_bytes
            {
                return Err(StoreError::CorruptMetadata(
                    "purge replacement metrics disagree with verified files",
                ));
            }
        }
        Ok(())
    }

    fn pack_object_ids_excluding_purge(
        &self,
        purge_id: u64,
        pack_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT location.object_id
             FROM object_locations AS location
             LEFT JOIN purge_objects AS selected
               ON selected.purge_id = ?1 AND selected.object_id = location.object_id
             WHERE location.pack_id = ?2
               AND location.verification_state = 'verified'
               AND selected.object_id IS NULL
             ORDER BY location.object_id LIMIT ?3",
        )?;
        let ids = statement
            .query_map(
                params![
                    to_sql_integer(purge_id)?,
                    pack_id,
                    i64::from(MAX_SUPPORTED_PACK_OBJECTS) + 1
                ],
                |row| row.get(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        if ids.len() > MAX_SUPPORTED_PACK_OBJECTS as usize {
            return Err(StoreError::PackLimitExceeded);
        }
        Ok(ids)
    }

    fn validate_physical_work_snapshot(&self, purge_id: u64) -> Result<(), StoreError> {
        let raw_purge_id = to_sql_integer(purge_id)?;
        let unmatched_loose: i64 = self.connection.query_row(
            "SELECT COUNT(*)
             FROM object_locations AS location
             JOIN purge_objects AS selected
               ON selected.purge_id = ?1 AND selected.object_id = location.object_id
             LEFT JOIN purge_file_work AS work
               ON work.purge_id = selected.purge_id
              AND work.location_id = location.location_id
              AND work.file_kind = 'loose'
             WHERE location.storage_kind = 'loose'
               AND location.verification_state = 'verified'
               AND work.location_id IS NULL",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        let unmatched_packs: i64 = self.connection.query_row(
            "SELECT COUNT(DISTINCT location.pack_id)
             FROM object_locations AS location
             JOIN purge_objects AS selected
               ON selected.purge_id = ?1 AND selected.object_id = location.object_id
             LEFT JOIN purge_pack_work AS work
               ON work.purge_id = selected.purge_id
              AND work.old_pack_id = location.pack_id
             WHERE location.storage_kind = 'pack'
               AND location.verification_state = 'verified'
               AND work.old_pack_id IS NULL",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        if unmatched_loose != 0 || unmatched_packs != 0 {
            return Err(StoreError::CorruptMetadata(
                "purge physical inventory changed after authorization",
            ));
        }
        Ok(())
    }

    fn retire_next_file_group(&mut self, purge_id: u64) -> Result<bool, StoreError> {
        if let Some(loose) = self.next_loose_file_work(purge_id)? {
            self.retire_loose_file(purge_id, loose)?;
            return Ok(true);
        }
        let old_pack_id: Option<String> = self
            .connection
            .query_row(
                "SELECT old_pack_id FROM purge_file_work
                 WHERE purge_id = ?1 AND file_kind IN ('pack', 'index')
                   AND state != 'retired'
                 ORDER BY (state = 'unlinking') DESC, old_pack_id LIMIT 1",
                [to_sql_integer(purge_id)?],
                |row| row.get(0),
            )
            .optional()?;
        let Some(old_pack_id) = old_pack_id else {
            return Ok(false);
        };
        self.retire_pack_file_group(purge_id, &old_pack_id)?;
        Ok(true)
    }

    fn next_loose_file_work(&self, purge_id: u64) -> Result<Option<LooseFileWork>, StoreError> {
        self.connection
            .query_row(
                "SELECT relative_path, location_id, object_id,
                        expected_file_bytes, state
                 FROM purge_file_work
                 WHERE purge_id = ?1 AND file_kind = 'loose'
                   AND state != 'retired'
                 ORDER BY (state = 'unlinking') DESC, relative_path LIMIT 1",
                [to_sql_integer(purge_id)?],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?
            .map(|(path, location_id, id, bytes, state)| {
                Ok(LooseFileWork {
                    relative_path: path,
                    location_id,
                    object_id: id.parse().map_err(|_| {
                        StoreError::CorruptMetadata("invalid cleanup loose object identity")
                    })?,
                    expected_file_bytes: sql_u64(bytes, "invalid cleanup loose byte count")?,
                    state,
                })
            })
            .transpose()
    }

    fn retire_loose_file(
        &mut self,
        purge_id: u64,
        mut work: LooseFileWork,
    ) -> Result<(), StoreError> {
        let path = loose_database_path(&work.relative_path)?;
        if path != loose_relative_path(work.object_id) {
            return Err(StoreError::CorruptMetadata(
                "cleanup loose path disagrees with object identity",
            ));
        }
        if work.state == "pending" {
            let pinned = PinnedManagedFile::open(&self.root, &path)?.ok_or(
                StoreError::CorruptMetadata("cleanup loose file disappeared before unlink intent"),
            )?;
            self.verify_obsolete_loose_work(&work, &pinned.file)?;
            let observed = pinned.length;
            if observed != work.expected_file_bytes {
                return Err(StoreError::CorruptMetadata(
                    "cleanup loose length changed before unlink",
                ));
            }
            let now = unix_time()?;
            let changed = self.connection.execute(
                "UPDATE purge_file_work
                 SET state = 'unlinking', observed_file_bytes = ?4,
                     unlink_started_at = ?5
                 WHERE purge_id = ?1 AND file_kind = 'loose'
                   AND relative_path = ?2 AND location_id = ?3 AND state = 'pending'",
                params![
                    to_sql_integer(purge_id)?,
                    work.relative_path,
                    work.location_id,
                    to_sql_integer(observed)?,
                    now
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::CorruptMetadata(
                    "cleanup loose unlink intent changed concurrently",
                ));
            }
            work.state = "unlinking".to_owned();
        }
        if work.state != "unlinking" {
            return Err(StoreError::CorruptMetadata(
                "invalid cleanup loose file state",
            ));
        }
        if let Some(pinned) = PinnedManagedFile::open(&self.root, &path)? {
            if pinned.length != work.expected_file_bytes {
                return Err(StoreError::CorruptMetadata(
                    "cleanup loose length changed before unlink",
                ));
            }
            self.verify_obsolete_loose_work(&work, &pinned.file)?;
            pinned.unlink()?;
        }
        let now = unix_time()?;
        let changed = self.connection.execute(
            "UPDATE purge_file_work SET state = 'retired', retired_at = ?4
             WHERE purge_id = ?1 AND file_kind = 'loose'
               AND relative_path = ?2 AND location_id = ?3 AND state = 'unlinking'",
            params![
                to_sql_integer(purge_id)?,
                work.relative_path,
                work.location_id,
                now
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::CorruptMetadata(
                "cleanup loose retirement changed concurrently",
            ));
        }
        Ok(())
    }

    fn verify_obsolete_loose_work(
        &self,
        work: &LooseFileWork,
        file: &File,
    ) -> Result<(), StoreError> {
        let (raw_kind, raw_length, state): (String, i64, String) = self.connection.query_row(
            "SELECT object.object_kind, object.uncompressed_length,
                    location.verification_state
             FROM content_objects AS object
             JOIN object_locations AS location USING (object_id)
             WHERE location.location_id = ?1 AND object.object_id = ?2
               AND location.storage_kind = 'loose'
               AND location.relative_path = ?3",
            params![
                work.location_id,
                work.object_id.to_string(),
                work.relative_path
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if state != "obsolete" {
            return Err(StoreError::CorruptMetadata(
                "cleanup loose location is not logically absent",
            ));
        }
        let kind = ObjectKind::from_database(&raw_kind)?;
        let expected_length = sql_u64(raw_length, "invalid cleanup loose object length")?;
        let mut pinned = file.try_clone()?;
        pinned.seek(SeekFrom::Start(0))?;
        let decoder = zstd::stream::read::Decoder::new(pinned)?;
        let bytes = read_bounded(decoder, expected_length)?;
        verify_object_bytes(work.object_id, kind, expected_length, &bytes)?;
        Ok(())
    }

    fn retire_pack_file_group(
        &mut self,
        purge_id: u64,
        old_pack_id: &str,
    ) -> Result<(), StoreError> {
        let mut files = self.pack_file_work(purge_id, old_pack_id)?;
        if files.len() != 2 {
            return Err(StoreError::CorruptMetadata(
                "cleanup pack does not have exact pack/index work",
            ));
        }
        let has_pending = files.iter().any(|file| file.state == "pending");
        let has_unlinking = files.iter().any(|file| file.state == "unlinking");
        if has_pending && has_unlinking {
            return Err(StoreError::CorruptMetadata(
                "cleanup pack files disagree on unlink intent",
            ));
        }
        if has_pending {
            self.verify_obsolete_pack_work(old_pack_id, &files)?;
            let observed = files
                .iter()
                .map(|file| {
                    let path = pack_database_path(
                        &file.relative_path,
                        if file.file_kind == "pack" {
                            ".pack"
                        } else {
                            ".idx"
                        },
                    )?;
                    let pinned = PinnedManagedFile::open(&self.root, &path)?.ok_or(
                        StoreError::CorruptMetadata(
                            "cleanup pack file disappeared before unlink intent",
                        ),
                    )?;
                    if pinned.length != file.expected_file_bytes
                        || format!(
                            "b3:{}",
                            blake3::Hash::from_bytes(pinned.checksum()?).to_hex()
                        ) != file.expected_checksum
                    {
                        return Err(StoreError::CorruptPack(
                            "cleanup pack file changed before unlink intent",
                        ));
                    }
                    Ok(pinned.length)
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            if files
                .iter()
                .zip(&observed)
                .any(|(file, observed)| file.expected_file_bytes != *observed)
            {
                return Err(StoreError::CorruptMetadata(
                    "cleanup pack file length changed before unlink",
                ));
            }
            let now = unix_time()?;
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            for (file, observed) in files.iter_mut().zip(observed) {
                let changed = transaction.execute(
                    "UPDATE purge_file_work
                     SET state = 'unlinking', observed_file_bytes = ?5,
                         unlink_started_at = ?6
                     WHERE purge_id = ?1 AND old_pack_id = ?2
                       AND file_kind = ?3 AND relative_path = ?4
                       AND state = 'pending'",
                    params![
                        to_sql_integer(purge_id)?,
                        old_pack_id,
                        file.file_kind,
                        file.relative_path,
                        to_sql_integer(observed)?,
                        now
                    ],
                )?;
                if changed != 1 {
                    return Err(StoreError::CorruptMetadata(
                        "cleanup pack unlink intent changed concurrently",
                    ));
                }
                file.state = "unlinking".to_owned();
            }
            transaction.commit()?;
        }
        for file in &files {
            if file.state != "unlinking" {
                return Err(StoreError::CorruptMetadata(
                    "invalid cleanup pack file state",
                ));
            }
            let extension = if file.file_kind == "pack" {
                ".pack"
            } else {
                ".idx"
            };
            let path = pack_database_path(&file.relative_path, extension)?;
            if let Some(pinned) = PinnedManagedFile::open(&self.root, &path)? {
                if pinned.length != file.expected_file_bytes {
                    return Err(StoreError::CorruptPack(
                        "cleanup pack file length changed before unlink",
                    ));
                }
                if format!(
                    "b3:{}",
                    blake3::Hash::from_bytes(pinned.checksum()?).to_hex()
                ) != file.expected_checksum
                {
                    return Err(StoreError::CorruptPack(
                        "cleanup pack file checksum changed before unlink",
                    ));
                }
                pinned.unlink()?;
            }
        }
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE purge_file_work SET state = 'retired', retired_at = ?3
             WHERE purge_id = ?1 AND old_pack_id = ?2
               AND file_kind IN ('pack', 'index') AND state = 'unlinking'",
            params![to_sql_integer(purge_id)?, old_pack_id, now],
        )?;
        if changed != 2 {
            return Err(StoreError::CorruptMetadata(
                "cleanup pack retirement count disagrees",
            ));
        }
        let changed = transaction.execute(
            "UPDATE purge_pack_work SET state = 'retired'
             WHERE purge_id = ?1 AND old_pack_id = ?2
               AND state = 'replacement-ready'",
            params![to_sql_integer(purge_id)?, old_pack_id],
        )?;
        if changed != 1 {
            return Err(StoreError::CorruptMetadata(
                "cleanup pack journal was not replacement-ready",
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    fn pack_file_work(
        &self,
        purge_id: u64,
        old_pack_id: &str,
    ) -> Result<Vec<PackFileWork>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT file_kind, relative_path, expected_checksum,
                    expected_file_bytes, state
             FROM purge_file_work
             WHERE purge_id = ?1 AND old_pack_id = ?2
               AND file_kind IN ('pack', 'index')
               AND state != 'retired'
             ORDER BY file_kind LIMIT 3",
        )?;
        let rows = statement
            .query_map(params![to_sql_integer(purge_id)?, old_pack_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(kind, path, checksum, bytes, state)| {
                Ok(PackFileWork {
                    file_kind: kind,
                    relative_path: path,
                    expected_checksum: checksum,
                    expected_file_bytes: sql_u64(bytes, "invalid cleanup pack byte count")?,
                    state,
                })
            })
            .collect()
    }

    fn verify_obsolete_pack_work(
        &self,
        old_pack_id: &str,
        files: &[PackFileWork],
    ) -> Result<(), StoreError> {
        let (generation, object_count, state): (i64, i64, String) = self.connection.query_row(
            "SELECT generation, object_count, state FROM packs WHERE pack_id = ?1",
            [old_pack_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if state != "obsolete" {
            return Err(StoreError::CorruptMetadata(
                "cleanup old pack is not logically obsolete",
            ));
        }
        let pack = files
            .iter()
            .find(|file| file.file_kind == "pack")
            .ok_or(StoreError::CorruptMetadata("cleanup pack work is missing"))?;
        let index = files
            .iter()
            .find(|file| file.file_kind == "index")
            .ok_or(StoreError::CorruptMetadata("cleanup index work is missing"))?;
        let pack_path = pack_database_path(&pack.relative_path, ".pack")?;
        let index_path = pack_database_path(&index.relative_path, ".idx")?;
        validate_managed_file_ancestors(&self.root, &pack_path)?;
        validate_managed_file_ancestors(&self.root, &index_path)?;
        verify_pack_files(
            &self.root.join(pack_path),
            &self.root.join(index_path),
            parse_checksum(&pack.expected_checksum)?,
            parse_checksum(&index.expected_checksum)?,
            sql_u64(generation, "invalid cleanup pack generation")?,
            self.config.max_object_bytes,
            sql_u64(object_count, "invalid cleanup pack object count")?,
        )
    }

    fn finish_purge_cleanup(&mut self, purge_id: u64) -> Result<(), StoreError> {
        let event = self.validate_authenticated_purge(purge_id)?;
        let raw_purge_id = to_sql_integer(purge_id)?;
        let remaining_files: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM purge_file_work
             WHERE purge_id = ?1 AND state != 'retired'",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        let remaining_packs: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM purge_pack_work
             WHERE purge_id = ?1 AND state != 'retired'",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        let active_targets: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM object_locations AS location
             JOIN purge_objects AS selected USING (object_id)
             WHERE selected.purge_id = ?1
               AND location.verification_state = 'verified'",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        let absence_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM purge_authorized_absences WHERE purge_id = ?1",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        if remaining_files != 0
            || remaining_packs != 0
            || active_targets != 0
            || sql_u64(absence_count, "invalid authorized absence count")? != event.object_count
        {
            return Err(StoreError::CorruptMetadata(
                "purge cleanup cannot finish with incomplete retirement",
            ));
        }
        let retired_bytes: i64 = self.connection.query_row(
            "SELECT COALESCE(SUM(observed_file_bytes), 0)
             FROM purge_file_work WHERE purge_id = ?1 AND state = 'retired'",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        let replacement_bytes: i64 = self.connection.query_row(
            "SELECT COALESCE(SUM(pack_bytes + index_bytes), 0)
             FROM purge_replacement_metrics WHERE purge_id = ?1",
            [raw_purge_id],
            |row| row.get(0),
        )?;
        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE purge_cleanup_accounting
             SET retired_file_bytes = ?2, replacement_file_bytes = ?3,
                 directories_synced_at = ?4, completed_at = ?4
             WHERE purge_id = ?1 AND completed_at IS NULL",
            params![raw_purge_id, retired_bytes, replacement_bytes, now],
        )?;
        if changed != 1 {
            return Err(StoreError::CorruptMetadata(
                "purge cleanup accounting was already finalized",
            ));
        }
        let changed = transaction.execute(
            "UPDATE purge_operations
             SET state = 'succeeded', updated_at = ?2, finished_at = ?2
             WHERE purge_id = ?1 AND state = 'cleaning'",
            params![raw_purge_id, now],
        )?;
        if changed != 1 {
            return Err(StoreError::CorruptMetadata(
                "purge journal did not finish from cleaning",
            ));
        }
        transaction.commit()?;
        Ok(())
    }
}

fn validate_managed_file_ancestors(root: &Path, relative: &Path) -> Result<(), StoreError> {
    let parent = relative.parent().ok_or(StoreError::CorruptMetadata(
        "purge target path lacks a managed parent",
    ))?;
    validate_managed_directory(root, parent)
}

fn validate_managed_directory(root: &Path, relative: &Path) -> Result<(), StoreError> {
    let root_metadata = fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(StoreError::CorruptMetadata(
            "purge managed root is symlinked or not a directory",
        ));
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(StoreError::CorruptMetadata(
                "purge managed path contains a non-normal component",
            ));
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StoreError::CorruptMetadata(
                "purge managed ancestor is symlinked or not a directory",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
struct PinnedManagedFile {
    parent: rustix::fd::OwnedFd,
    leaf_name: std::ffi::OsString,
    file: File,
    device: u64,
    inode: u64,
    length: u64,
}

#[cfg(unix)]
impl PinnedManagedFile {
    fn open(root: &Path, relative: &Path) -> Result<Option<Self>, StoreError> {
        let leaf_name = relative
            .file_name()
            .ok_or(StoreError::CorruptMetadata(
                "purge target path lacks a managed leaf",
            ))?
            .to_os_string();
        let parent_path = relative.parent().ok_or(StoreError::CorruptMetadata(
            "purge target path lacks a managed parent",
        ))?;
        let directory_flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC;
        let mut parent = rustix::fs::open(root, directory_flags, rustix::fs::Mode::empty())
            .map_err(managed_traversal_error)?;
        for component in parent_path.components() {
            let Component::Normal(component) = component else {
                return Err(StoreError::CorruptMetadata(
                    "purge managed path contains a non-normal component",
                ));
            };
            parent = rustix::fs::openat(
                &parent,
                component,
                directory_flags,
                rustix::fs::Mode::empty(),
            )
            .map_err(managed_traversal_error)?;
        }
        let file_flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC;
        let file_descriptor =
            match rustix::fs::openat(&parent, &leaf_name, file_flags, rustix::fs::Mode::empty()) {
                Ok(file) => file,
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(error) => return Err(managed_leaf_error(error)),
            };
        let file: File = file_descriptor.into();
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(StoreError::CorruptMetadata(
                "purge target path is not a regular file",
            ));
        }
        Ok(Some(Self {
            parent,
            leaf_name,
            device: std::os::unix::fs::MetadataExt::dev(&metadata),
            inode: std::os::unix::fs::MetadataExt::ino(&metadata),
            length: metadata.len(),
            file,
        }))
    }

    fn checksum(&self) -> Result<[u8; 32], StoreError> {
        checksum_file(&mut self.file.try_clone()?)
    }

    fn unlink(self) -> Result<(), StoreError> {
        let confirm_descriptor = rustix::fs::openat(
            &self.parent,
            &self.leaf_name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(managed_leaf_error)?;
        let confirm_file: File = confirm_descriptor.into();
        let metadata = confirm_file.metadata()?;
        if !metadata.is_file()
            || std::os::unix::fs::MetadataExt::dev(&metadata) != self.device
            || std::os::unix::fs::MetadataExt::ino(&metadata) != self.inode
            || metadata.len() != self.length
        {
            return Err(StoreError::CorruptMetadata(
                "purge target leaf changed immediately before unlink",
            ));
        }
        rustix::fs::unlinkat(&self.parent, &self.leaf_name, rustix::fs::AtFlags::empty())
            .map_err(|error| StoreError::Io(io::Error::from(error)))?;
        rustix::fs::fsync(&self.parent).map_err(|error| StoreError::Io(io::Error::from(error)))?;
        Ok(())
    }
}

#[cfg(unix)]
fn managed_traversal_error(error: rustix::io::Errno) -> StoreError {
    match error {
        rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => {
            StoreError::CorruptMetadata("purge managed ancestor is symlinked or not a directory")
        }
        _ => StoreError::Io(io::Error::from(error)),
    }
}

#[cfg(unix)]
fn managed_leaf_error(error: rustix::io::Errno) -> StoreError {
    match error {
        rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => {
            StoreError::CorruptMetadata("purge target path is not a regular file")
        }
        _ => StoreError::Io(io::Error::from(error)),
    }
}

pub(super) fn checked_regular_file_length(path: &Path) -> Result<u64, StoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreError::CorruptMetadata(
            "purge target path is not a regular file",
        ));
    }
    Ok(metadata.len())
}

fn regular_file_exists(path: &Path) -> Result<bool, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            StoreError::CorruptMetadata("purge target path is not a regular file"),
        ),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StoreError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture_cleanup_page(
        library: &mut Library,
        wiki_id: WikiId,
        collection_id: CollectionId,
        page_id: u64,
        revision_id: u64,
        page_title: &str,
        source: &[u8],
    ) -> ObjectId {
        let title = PageTitle::new(page_title).expect("fixture title");
        library
            .capture_current_revision(
                wiki_id,
                collection_id,
                &CurrentRevisionCapture {
                    page_id: PageId::new(page_id).expect("fixture page ID"),
                    namespace: 0,
                    title: &title,
                    revision_id: RevisionId::new(revision_id).expect("fixture revision ID"),
                    parent_id: None,
                    timestamp: "2026-08-24T10:00:00Z",
                    author: None,
                    author_id: None,
                    comment: None,
                    minor: false,
                    upstream_sha1: None,
                    content_model: "wikitext",
                    source,
                },
            )
            .expect("capture fixture page")
            .id
    }

    #[test]
    fn cleanup_schema_is_strict_and_rejects_unbacked_absence() {
        let directory = tempfile::tempdir().expect("temporary library");
        let library = Library::open(directory.path()).expect("open library");
        let sql: String = library
            .connection
            .query_row(
                "SELECT sql FROM sqlite_schema
                 WHERE type = 'table' AND name = 'purge_file_work'",
                [],
                |row| row.get(0),
            )
            .expect("cleanup schema");
        assert!(sql.ends_with("STRICT"));
        assert!(
            library
                .connection
                .execute(
                    "INSERT INTO purge_authorized_absences
                     (purge_id, object_id, absent_at) VALUES (1, 'b3:missing', 0)",
                    [],
                )
                .is_err(),
            "an absence must be backed by the exact purge inventory"
        );
    }

    #[test]
    fn version_fourteen_upgrade_preserves_purge_journal_and_event() {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("open library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let target = library
            .create_explicit_collection(wiki_id, "Migration purge target")
            .expect("target collection");
        capture_cleanup_page(
            &mut library,
            wiki_id,
            target,
            10,
            100,
            "Migration purge fixture",
            b"migration purge payload",
        );
        library
            .tombstone_collection(target)
            .expect("tombstone target");
        let preview = library.preview_collection_purge(target).expect("preview");
        let receipt = library
            .authorize_collection_purge(
                target,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize purge");
        let event = library
            .append_purge_manifest(receipt.purge_id)
            .expect("append purge event")
            .manifest
            .purge()
            .expect("typed purge event")
            .clone();
        library
            .connection
            .execute_batch(
                "DROP TABLE whole_edition_discovery_changes;
                 DROP TABLE whole_edition_changes;
                 DROP TABLE whole_edition_discoveries;
                 DROP TABLE whole_edition_imports;
                 DROP TABLE whole_edition_recovery_markers;
                 DROP TABLE purge_cleanup_accounting;
                 DROP TABLE purge_replacement_metrics;
                 DROP TABLE purge_file_work;
                 DROP TABLE purge_authorized_absences;
                 ALTER TABLE purge_operations DROP COLUMN catalog_fingerprint;
                 DELETE FROM schema_migrations WHERE version IN (15, 16, 17);
                 PRAGMA user_version = 14;",
            )
            .expect("restore version-fourteen schema shape");
        drop(library);

        let mut upgraded = Library::open(directory.path()).expect("upgrade library");
        assert_eq!(upgraded.schema_version().expect("schema version"), 17);
        assert_eq!(
            upgraded
                .installed_purge_event(receipt.purge_id)
                .expect("manifest lookup")
                .expect("retained purge event"),
            event
        );
        let (legacy_state, legacy_catalog): (String, Option<String>) = upgraded
            .connection
            .query_row(
                "SELECT state, catalog_fingerprint FROM purge_operations
                 WHERE purge_id = ?1",
                [to_sql_integer(receipt.purge_id).expect("purge ID")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("legacy purge journal");
        assert_eq!(legacy_state, "failed");
        assert_eq!(legacy_catalog, None);
        assert!(matches!(
            upgraded.purge_verification_snapshot(receipt.purge_id),
            Err(StoreError::PurgeCatalogCommitmentMissing(id)) if id == receipt.purge_id
        ));
        for table in [
            "purge_authorized_absences",
            "purge_file_work",
            "purge_replacement_metrics",
            "purge_cleanup_accounting",
        ] {
            let count: i64 = upgraded
                .connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("new cleanup table count");
            assert_eq!(count, 0, "{table} starts empty after migration");
        }
        let sql: String = upgraded
            .connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name = 'purge_file_work'",
                [],
                |row| row.get(0),
            )
            .expect("strict cleanup table");
        assert!(sql.ends_with("STRICT"));

        let replacement_preview = upgraded
            .preview_collection_purge(target)
            .expect("fresh committed-catalog preview");
        let replacement = upgraded
            .authorize_collection_purge(
                target,
                PurgeAuthorization {
                    collection_name: &replacement_preview.collection_name,
                    preview_fingerprint: &replacement_preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize replacement purge");
        assert_ne!(replacement.purge_id, receipt.purge_id);
    }

    #[test]
    fn unfinished_cleanup_page_bounds_are_enforced() {
        let directory = tempfile::tempdir().expect("temporary library");
        let library = Library::open(directory.path()).expect("open library");
        assert!(matches!(
            library.unfinished_purge_cleanups(None, 0),
            Err(StoreError::InvalidConfig(_))
        ));
        assert!(matches!(
            library.unfinished_purge_cleanups(None, MAX_UNFINISHED_PURGE_PAGE_SIZE + 1),
            Err(StoreError::InvalidConfig(_))
        ));
    }

    #[test]
    fn loose_only_cleanup_advances_through_restartable_checkpoints() {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("open library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Cleanup target")
            .expect("create collection");
        let object_id = capture_cleanup_page(
            &mut library,
            wiki_id,
            collection_id,
            10,
            100,
            "Cleanup fixture",
            b"payload selected for cleanup",
        );
        let relative_path: String = library
            .connection
            .query_row(
                "SELECT relative_path FROM object_locations
                 WHERE object_id = ?1 AND storage_kind = 'loose'",
                [object_id.to_string()],
                |row| row.get(0),
            )
            .expect("loose path");
        library
            .tombstone_collection(collection_id)
            .expect("tombstone collection");
        let preview = library
            .preview_collection_purge(collection_id)
            .expect("purge preview");
        let receipt = library
            .authorize_collection_purge(
                collection_id,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize purge");

        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("prepare cleanup")
                .step,
            PurgeCleanupStep::Prepared
        );
        assert_eq!(
            library
                .verify_purge_cleanup_state(receipt.purge_id)
                .expect("verify repacking cleanup")
                .state,
            PurgeJournalState::Repacking
        );
        assert!(library.root.join(&relative_path).exists());
        drop(library);

        let mut library = Library::open(directory.path()).expect("reopen prepared cleanup");
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("commit authorized absence")
                .step,
            PurgeCleanupStep::AuthorizedAbsenceCommitted
        );
        assert!(library.root.join(&relative_path).exists());
        assert!(matches!(
            library.read_object(object_id),
            Err(StoreError::ObjectNotFound(id)) if id == object_id
        ));
        assert_eq!(
            library
                .purge_authorized_absence(object_id)
                .expect("absence lookup")
                .expect("authorized absence")
                .purge_id,
            receipt.purge_id
        );
        assert_eq!(
            library
                .verify_purge_cleanup_state(receipt.purge_id)
                .expect("verify cleaning cleanup")
                .state,
            PurgeJournalState::Cleaning
        );
        drop(library);

        let mut library = Library::open(directory.path()).expect("reopen logical cleanup");
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("retire loose file")
                .step,
            PurgeCleanupStep::FilesRetired
        );
        assert!(!library.root.join(&relative_path).exists());
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("finish cleanup")
                .step,
            PurgeCleanupStep::Completed
        );
        let progress = library
            .purge_cleanup_progress(receipt.purge_id)
            .expect("completed progress");
        assert_eq!(progress.state, PurgeJournalState::Succeeded);
        assert_eq!(progress.retired_file_count, 1);
        assert!(progress.retired_file_bytes > 0);
        assert_eq!(
            library
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM purge_authorized_absences
                     WHERE purge_id = ?1 AND object_id = ?2",
                    params![
                        to_sql_integer(receipt.purge_id).expect("purge ID"),
                        object_id.to_string()
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .expect("absence count"),
            1
        );
        assert_eq!(
            library
                .verify_purge_cleanup_state(receipt.purge_id)
                .expect("verify completed cleanup")
                .state,
            PurgeJournalState::Succeeded
        );
        library
            .connection
            .execute(
                "UPDATE purge_file_work
                 SET expected_file_bytes = expected_file_bytes + 1
                 WHERE purge_id = ?1 AND file_kind = 'loose'",
                [to_sql_integer(receipt.purge_id).expect("purge ID")],
            )
            .expect("tamper expected cleanup bytes");
        assert!(matches!(
            library.verify_purge_cleanup_state(receipt.purge_id),
            Err(StoreError::CorruptMetadata(
                "observed cleanup file bytes disagree with prepared inventory"
            ))
        ));
        library
            .connection
            .execute(
                "UPDATE purge_file_work
                 SET expected_file_bytes = observed_file_bytes
                 WHERE purge_id = ?1 AND file_kind = 'loose'",
                [to_sql_integer(receipt.purge_id).expect("purge ID")],
            )
            .expect("restore expected cleanup bytes");
        library
            .connection
            .execute(
                "UPDATE purge_cleanup_accounting
                 SET retired_file_bytes = retired_file_bytes + 1
                 WHERE purge_id = ?1",
                [to_sql_integer(receipt.purge_id).expect("purge ID")],
            )
            .expect("tamper cleanup accounting");
        assert!(matches!(
            library.verify_purge_cleanup_state(receipt.purge_id),
            Err(StoreError::CorruptMetadata(
                "completed purge accounting disagrees with cleanup work"
            ))
        ));
    }

    #[test]
    fn whole_pack_is_verified_before_becoming_retirable() {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("open library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Whole-pack target")
            .expect("create collection");
        let object_id = capture_cleanup_page(
            &mut library,
            wiki_id,
            collection_id,
            10,
            100,
            "Whole-pack fixture",
            b"whole pack payload",
        );
        let pack = library
            .pack_loose_objects()
            .expect("pack candidates")
            .expect("whole pack");
        let pack_path: String = library
            .connection
            .query_row(
                "SELECT pack_path FROM packs WHERE pack_id = ?1",
                [&pack.pack_id],
                |row| row.get(0),
            )
            .expect("pack path");
        library
            .tombstone_collection(collection_id)
            .expect("tombstone collection");
        let preview = library
            .preview_collection_purge(collection_id)
            .expect("purge preview");
        assert_eq!(preview.whole_pack_count, 1);
        let receipt = library
            .authorize_collection_purge(
                collection_id,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize purge");

        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("prepare")
                .step,
            PurgeCleanupStep::Prepared
        );
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("verify whole pack")
                .step,
            PurgeCleanupStep::WholePackReady
        );
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("commit absence")
                .step,
            PurgeCleanupStep::AuthorizedAbsenceCommitted
        );
        assert!(library.root.join(&pack_path).exists());
        assert!(matches!(
            library.read_object(object_id),
            Err(StoreError::ObjectNotFound(id)) if id == object_id
        ));

        while library
            .purge_cleanup_progress(receipt.purge_id)
            .expect("cleanup progress")
            .pending_file_count
            + library
                .purge_cleanup_progress(receipt.purge_id)
                .expect("cleanup progress")
                .unlinking_file_count
            > 0
        {
            assert_eq!(
                library
                    .resume_purge_cleanup(receipt.purge_id)
                    .expect("retire file group")
                    .step,
                PurgeCleanupStep::FilesRetired
            );
        }
        assert!(!library.root.join(&pack_path).exists());
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("finish")
                .step,
            PurgeCleanupStep::Completed
        );
    }

    #[test]
    fn mixed_pack_replacement_is_exact_restartable_and_precedes_retirement() {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("open library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let target = library
            .create_explicit_collection(wiki_id, "Mixed-pack target")
            .expect("target collection");
        let retained = library
            .create_explicit_collection(wiki_id, "Mixed-pack retained")
            .expect("retained collection");
        let target_id = capture_cleanup_page(
            &mut library,
            wiki_id,
            target,
            10,
            100,
            "Target fixture",
            b"target mixed-pack payload",
        );
        let retained_id = capture_cleanup_page(
            &mut library,
            wiki_id,
            retained,
            20,
            200,
            "Retained fixture",
            b"retained mixed-pack payload",
        );
        let pack = library
            .pack_loose_objects()
            .expect("pack candidates")
            .expect("mixed pack");
        library
            .prune_packed_loose_objects(&pack.pack_id)
            .expect("remove loose fallbacks");
        let old_pack_path: String = library
            .connection
            .query_row(
                "SELECT pack_path FROM packs WHERE pack_id = ?1",
                [&pack.pack_id],
                |row| row.get(0),
            )
            .expect("old pack path");
        library
            .tombstone_collection(target)
            .expect("tombstone target");
        let preview = library.preview_collection_purge(target).expect("preview");
        assert_eq!(preview.mixed_pack_count, 1);
        let receipt = library
            .authorize_collection_purge(
                target,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize purge");

        library
            .resume_purge_cleanup(receipt.purge_id)
            .expect("prepare cleanup");
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("build replacement")
                .step,
            PurgeCleanupStep::ReplacementReady
        );
        assert_eq!(
            library.read_object(target_id).expect("target retained"),
            b"target mixed-pack payload"
        );
        assert_eq!(
            library.read_object(retained_id).expect("neighbor retained"),
            b"retained mixed-pack payload"
        );
        assert_eq!(
            library
                .connection
                .query_row(
                    "SELECT state FROM packs WHERE pack_id = ?1",
                    [&pack.pack_id],
                    |row| row.get::<_, String>(0),
                )
                .expect("pack state"),
            "verified"
        );
        assert_eq!(
            library
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM purge_authorized_absences WHERE purge_id = ?1",
                    [to_sql_integer(receipt.purge_id).expect("purge ID")],
                    |row| row.get::<_, i64>(0),
                )
                .expect("absence count"),
            0
        );

        let replacement_pack_id: String = library
            .connection
            .query_row(
                "SELECT replacement_pack_id FROM purge_pack_work
                 WHERE purge_id = ?1 AND old_pack_id = ?2",
                params![
                    to_sql_integer(receipt.purge_id).expect("purge ID"),
                    pack.pack_id
                ],
                |row| row.get(0),
            )
            .expect("replacement pack ID");
        assert_ne!(replacement_pack_id, pack.pack_id);
        let replacement_ids = library
            .pack_object_ids_excluding_purge(receipt.purge_id, &replacement_pack_id)
            .expect("replacement inventory");
        assert_eq!(replacement_ids, vec![retained_id.to_string()]);
        let (metric_pack_bytes, metric_index_bytes): (i64, i64) = library
            .connection
            .query_row(
                "SELECT pack_bytes, index_bytes FROM purge_replacement_metrics
                 WHERE purge_id = ?1 AND old_pack_id = ?2",
                params![
                    to_sql_integer(receipt.purge_id).expect("purge ID"),
                    pack.pack_id
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("replacement metrics");
        let replacement = library
            .verify_managed_recorded_pack(&replacement_pack_id)
            .expect("verify replacement pack");
        assert_eq!(replacement.object_count, 1);
        let replacement_pack_path = library.root.join(&replacement.pack_path);
        let replacement_index_path = library.root.join(&replacement.index_path);
        assert_eq!(
            sql_u64(metric_pack_bytes, "metric pack bytes").expect("pack metric"),
            checked_regular_file_length(&replacement_pack_path).expect("replacement pack length")
        );
        assert_eq!(
            sql_u64(metric_index_bytes, "metric index bytes").expect("index metric"),
            checked_regular_file_length(&replacement_index_path).expect("replacement index length")
        );
        let ready_progress = library
            .verify_purge_cleanup_state(receipt.purge_id)
            .expect("independently verify replacement-ready state");
        assert_eq!(ready_progress.replacement_ready_pack_count, 1);
        assert!(ready_progress.replacement_file_bytes > 0);

        let original_pack = fs::read(&replacement_pack_path).expect("replacement bytes");
        let mut tampered_pack = original_pack.clone();
        let last = tampered_pack
            .last_mut()
            .expect("replacement pack is nonempty");
        *last ^= 0x01;
        fs::write(&replacement_pack_path, tampered_pack).expect("tamper replacement pack");
        assert!(matches!(
            library.resume_purge_cleanup(receipt.purge_id),
            Err(StoreError::CorruptPack("pack checksum mismatch"))
        ));
        assert_eq!(
            library
                .connection
                .query_row(
                    "SELECT state FROM packs WHERE pack_id = ?1",
                    [&pack.pack_id],
                    |row| row.get::<_, String>(0),
                )
                .expect("old pack remains active after payload tamper"),
            "verified"
        );
        fs::write(&replacement_pack_path, original_pack).expect("restore replacement pack");

        library
            .connection
            .execute(
                "UPDATE purge_replacement_metrics SET pack_bytes = pack_bytes + 1
                 WHERE purge_id = ?1 AND old_pack_id = ?2",
                params![
                    to_sql_integer(receipt.purge_id).expect("purge ID"),
                    pack.pack_id
                ],
            )
            .expect("tamper replacement metric");
        assert!(matches!(
            library.resume_purge_cleanup(receipt.purge_id),
            Err(StoreError::CorruptMetadata(
                "purge replacement metrics disagree with verified files"
            ))
        ));
        assert_eq!(
            library
                .read_object(target_id)
                .expect("target remains active"),
            b"target mixed-pack payload"
        );
        assert_eq!(
            library
                .purge_cleanup_progress(receipt.purge_id)
                .expect("failed-advance progress")
                .state,
            PurgeJournalState::Repacking
        );
        assert_eq!(
            library
                .connection
                .query_row(
                    "SELECT state FROM packs WHERE pack_id = ?1",
                    [&pack.pack_id],
                    |row| row.get::<_, String>(0),
                )
                .expect("old pack remains active after metric tamper"),
            "verified"
        );
        library
            .connection
            .execute(
                "UPDATE purge_replacement_metrics SET pack_bytes = pack_bytes - 1
                 WHERE purge_id = ?1 AND old_pack_id = ?2",
                params![
                    to_sql_integer(receipt.purge_id).expect("purge ID"),
                    pack.pack_id
                ],
            )
            .expect("restore replacement metric");

        drop(library);
        let mut library = Library::open(directory.path()).expect("reopen after replacement");
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("commit absence after restart")
                .step,
            PurgeCleanupStep::AuthorizedAbsenceCommitted
        );
        assert!(matches!(
            library.read_object(target_id),
            Err(StoreError::ObjectNotFound(id)) if id == target_id
        ));
        assert_eq!(
            library.read_object(retained_id).expect("read replacement"),
            b"retained mixed-pack payload"
        );
        assert_eq!(
            library
                .connection
                .query_row(
                    "SELECT state FROM packs WHERE pack_id = ?1",
                    [&pack.pack_id],
                    |row| row.get::<_, String>(0),
                )
                .expect("obsolete old pack"),
            "obsolete"
        );
        assert!(library.root.join(&old_pack_path).exists());

        while library
            .purge_cleanup_progress(receipt.purge_id)
            .expect("cleanup progress")
            .pending_file_count
            + library
                .purge_cleanup_progress(receipt.purge_id)
                .expect("cleanup progress")
                .unlinking_file_count
            > 0
        {
            assert_eq!(
                library
                    .resume_purge_cleanup(receipt.purge_id)
                    .expect("retire old files")
                    .step,
                PurgeCleanupStep::FilesRetired
            );
        }
        assert!(!library.root.join(old_pack_path).exists());
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("finish mixed purge")
                .step,
            PurgeCleanupStep::Completed
        );
        assert_eq!(
            library
                .verify_purge_cleanup_state(receipt.purge_id)
                .expect("verify completed mixed purge")
                .state,
            PurgeJournalState::Succeeded
        );
        assert_eq!(
            library
                .read_object(retained_id)
                .expect("retained after cleanup"),
            b"retained mixed-pack payload"
        );
    }

    #[test]
    fn mixed_pack_replacement_respects_input_bound_without_advancing_state() {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("open library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let target = library
            .create_explicit_collection(wiki_id, "Bounded mixed target")
            .expect("target collection");
        let retained = library
            .create_explicit_collection(wiki_id, "Bounded mixed retained")
            .expect("retained collection");
        let target_id = capture_cleanup_page(
            &mut library,
            wiki_id,
            target,
            10,
            100,
            "Bounded target",
            b"target payload",
        );
        let retained_id = capture_cleanup_page(
            &mut library,
            wiki_id,
            retained,
            20,
            200,
            "Bounded retained",
            b"retained payload exceeds one byte",
        );
        let pack = library
            .pack_loose_objects()
            .expect("pack candidates")
            .expect("mixed pack");
        library
            .prune_packed_loose_objects(&pack.pack_id)
            .expect("remove loose fallbacks");
        library
            .tombstone_collection(target)
            .expect("tombstone target");
        let preview = library.preview_collection_purge(target).expect("preview");
        let receipt = library
            .authorize_collection_purge(
                target,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize purge");
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("prepare cleanup")
                .step,
            PurgeCleanupStep::Prepared
        );
        drop(library);

        let config = StoreConfig::default()
            .with_max_pack_input_bytes(1)
            .expect("one-byte pack bound");
        let mut library = Library::open_with_config(directory.path(), config)
            .expect("reopen with smaller pack bound");
        assert!(matches!(
            library.resume_purge_cleanup(receipt.purge_id),
            Err(StoreError::PackLimitExceeded)
        ));
        let progress = library
            .purge_cleanup_progress(receipt.purge_id)
            .expect("bounded replacement progress");
        assert_eq!(progress.state, PurgeJournalState::Repacking);
        assert_eq!(progress.pending_pack_count, 1);
        assert_eq!(progress.replacement_ready_pack_count, 0);
        assert_eq!(
            library
                .read_object(target_id)
                .expect("target remains active"),
            b"target payload"
        );
        assert_eq!(
            library
                .read_object(retained_id)
                .expect("retained remains active"),
            b"retained payload exceeds one byte"
        );
        assert_eq!(
            library
                .connection
                .query_row(
                    "SELECT state FROM packs WHERE pack_id = ?1",
                    [&pack.pack_id],
                    |row| row.get::<_, String>(0),
                )
                .expect("old pack remains active"),
            "verified"
        );
    }

    #[test]
    fn cleanup_fails_closed_when_an_object_becomes_shared_after_the_event() {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("open library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let target = library
            .create_explicit_collection(wiki_id, "Late-share target")
            .expect("target collection");
        let retained = library
            .create_explicit_collection(wiki_id, "Late-share retained")
            .expect("retained collection");
        let target_id = capture_cleanup_page(
            &mut library,
            wiki_id,
            target,
            10,
            100,
            "Late-share target",
            b"late shared payload",
        );
        library
            .tombstone_collection(target)
            .expect("tombstone target");
        let preview = library.preview_collection_purge(target).expect("preview");
        let receipt = library
            .authorize_collection_purge(
                target,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize purge");
        library
            .append_purge_manifest(receipt.purge_id)
            .expect("append purge event");

        let retained_id = capture_cleanup_page(
            &mut library,
            wiki_id,
            retained,
            20,
            200,
            "Late-share retained",
            b"late shared payload",
        );
        assert_eq!(target_id, retained_id);
        assert!(matches!(
            library.resume_purge_cleanup(receipt.purge_id),
            Err(StoreError::CorruptMetadata(
                "purge object inventory gained a retained reference"
            ))
        ));
        assert_eq!(
            library
                .purge_verification_snapshot(receipt.purge_id)
                .expect("verification snapshot")
                .state,
            PurgeJournalState::Authorized
        );
        assert_eq!(
            library.read_object(target_id).expect("shared bytes remain"),
            b"late shared payload"
        );
        assert_eq!(
            library
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM purge_file_work WHERE purge_id = ?1",
                    [to_sql_integer(receipt.purge_id).expect("purge ID")],
                    |row| row.get::<_, i64>(0),
                )
                .expect("file work count"),
            0
        );
    }

    #[test]
    fn cleanup_rejects_physical_catalog_drift_after_the_event() {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("open library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let target = library
            .create_explicit_collection(wiki_id, "Catalog-drift target")
            .expect("target collection");
        let object_id = capture_cleanup_page(
            &mut library,
            wiki_id,
            target,
            10,
            100,
            "Catalog drift",
            b"catalog drift payload",
        );
        library
            .tombstone_collection(target)
            .expect("tombstone target");
        let preview = library.preview_collection_purge(target).expect("preview");
        let receipt = library
            .authorize_collection_purge(
                target,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize purge");
        library
            .append_purge_manifest(receipt.purge_id)
            .expect("append purge event");
        let pack = library
            .pack_loose_objects()
            .expect("pack after event")
            .expect("new pack");

        assert!(matches!(
            library.resume_purge_cleanup(receipt.purge_id),
            Err(StoreError::StalePurgePreview(id)) if id == target
        ));
        assert_eq!(
            library.read_object(object_id).expect("payload remains"),
            b"catalog drift payload"
        );
        assert_eq!(
            library
                .connection
                .query_row(
                    "SELECT state FROM packs WHERE pack_id = ?1",
                    [&pack.pack_id],
                    |row| row.get::<_, String>(0),
                )
                .expect("pack state"),
            "verified"
        );
        assert_eq!(
            library
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM purge_file_work WHERE purge_id = ?1",
                    [to_sql_integer(receipt.purge_id).expect("purge ID")],
                    |row| row.get::<_, i64>(0),
                )
                .expect("file work count"),
            0
        );
    }

    #[test]
    fn managed_ancestor_validation_rejects_a_non_directory() {
        let directory = tempfile::tempdir().expect("temporary managed root");
        fs::write(directory.path().join("objects"), b"not a directory")
            .expect("create hostile ancestor");
        assert!(matches!(
            validate_managed_file_ancestors(
                directory.path(),
                Path::new("objects/loose/payload.zst")
            ),
            Err(StoreError::CorruptMetadata(
                "purge managed ancestor is symlinked or not a directory"
            ))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_unlink_stays_with_the_pinned_parent_after_directory_swap() {
        let directory = tempfile::tempdir().expect("temporary managed root");
        let original_parent = directory.path().join("objects/loose/original");
        fs::create_dir_all(&original_parent).expect("create original parent");
        fs::write(original_parent.join("payload.zst"), b"original").expect("write original leaf");
        let pinned = PinnedManagedFile::open(
            directory.path(),
            Path::new("objects/loose/original/payload.zst"),
        )
        .expect("open pinned payload")
        .expect("pinned payload");

        let displaced_parent = directory.path().join("objects/loose/displaced");
        fs::rename(&original_parent, &displaced_parent).expect("displace original parent");
        fs::create_dir(&original_parent).expect("install replacement parent");
        fs::write(original_parent.join("payload.zst"), b"replacement")
            .expect("write replacement leaf");

        pinned.unlink().expect("unlink through pinned parent");
        assert!(!displaced_parent.join("payload.zst").exists());
        assert_eq!(
            fs::read(original_parent.join("payload.zst")).expect("replacement survives"),
            b"replacement"
        );
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_unlink_rejects_a_leaf_swap_before_confirmation() {
        let directory = tempfile::tempdir().expect("temporary managed root");
        let parent = directory.path().join("objects/loose/pinned");
        fs::create_dir_all(&parent).expect("create parent");
        let leaf = parent.join("payload.zst");
        fs::write(&leaf, b"original").expect("write original leaf");
        let pinned = PinnedManagedFile::open(
            directory.path(),
            Path::new("objects/loose/pinned/payload.zst"),
        )
        .expect("open pinned payload")
        .expect("pinned payload");

        fs::rename(&leaf, parent.join("displaced.zst")).expect("swap original leaf out");
        fs::write(&leaf, b"replacement").expect("swap replacement leaf in");
        assert!(matches!(
            pinned.unlink(),
            Err(StoreError::CorruptMetadata(
                "purge target leaf changed immediately before unlink"
            ))
        ));
        assert_eq!(
            fs::read(&leaf).expect("replacement survives"),
            b"replacement"
        );
        assert_eq!(
            fs::read(parent.join("displaced.zst")).expect("original remains linked"),
            b"original"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_never_unlinks_through_a_symlinked_managed_ancestor() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("open library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Symlink ancestor target")
            .expect("create collection");
        let object_id = capture_cleanup_page(
            &mut library,
            wiki_id,
            collection_id,
            10,
            100,
            "Symlink ancestor fixture",
            b"payload protected from ancestor redirection",
        );
        let raw_path: String = library
            .connection
            .query_row(
                "SELECT relative_path FROM object_locations
                 WHERE object_id = ?1 AND storage_kind = 'loose'",
                [object_id.to_string()],
                |row| row.get(0),
            )
            .expect("loose path");
        library
            .tombstone_collection(collection_id)
            .expect("tombstone collection");
        let preview = library
            .preview_collection_purge(collection_id)
            .expect("preview purge");
        let receipt = library
            .authorize_collection_purge(
                collection_id,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize purge");
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("prepare cleanup")
                .step,
            PurgeCleanupStep::Prepared
        );
        assert_eq!(
            library
                .resume_purge_cleanup(receipt.purge_id)
                .expect("commit absence")
                .step,
            PurgeCleanupStep::AuthorizedAbsenceCommitted
        );

        let relative = loose_database_path(&raw_path).expect("validated loose path");
        let original_parent = library
            .root
            .join(&relative)
            .parent()
            .expect("loose parent")
            .to_path_buf();
        let redirected_parent = original_parent.with_extension("purge-hostile-target");
        fs::rename(&original_parent, &redirected_parent).expect("move managed ancestor");
        symlink(&redirected_parent, &original_parent).expect("install hostile ancestor symlink");

        assert!(matches!(
            library.resume_purge_cleanup(receipt.purge_id),
            Err(StoreError::CorruptMetadata(
                "purge managed ancestor is symlinked or not a directory"
            ))
        ));
        assert!(
            redirected_parent
                .join(relative.file_name().expect("loose file name"))
                .is_file(),
            "the redirected payload must not be deleted"
        );
    }

    #[test]
    fn succeeded_absence_can_be_superseded_by_identical_rehydrated_bytes() {
        let directory = tempfile::tempdir().expect("temporary library");
        let mut library = Library::open(directory.path()).expect("open library");
        let wiki_id = library
            .register_wiki("https://en.wikipedia.org/w/api.php", "en")
            .expect("register wiki");
        let collection_id = library
            .create_explicit_collection(wiki_id, "Rehydration target")
            .expect("create collection");
        let payload = b"identical payload reintroduced after succeeded purge";
        let object_id = capture_cleanup_page(
            &mut library,
            wiki_id,
            collection_id,
            10,
            100,
            "Rehydration fixture",
            payload,
        );
        library
            .tombstone_collection(collection_id)
            .expect("tombstone collection");
        let preview = library
            .preview_collection_purge(collection_id)
            .expect("preview purge");
        let receipt = library
            .authorize_collection_purge(
                collection_id,
                PurgeAuthorization {
                    collection_name: &preview.collection_name,
                    preview_fingerprint: &preview.fingerprint,
                    payload_only_acknowledged: true,
                    backups_not_erased_acknowledged: true,
                },
            )
            .expect("authorize purge");
        loop {
            if matches!(
                library
                    .resume_purge_cleanup(receipt.purge_id)
                    .expect("advance cleanup")
                    .step,
                PurgeCleanupStep::Completed | PurgeCleanupStep::AlreadyComplete
            ) {
                break;
            }
        }
        let rehydrated = library
            .put_bytes(ObjectKind::Wikitext, payload)
            .expect("rehydrate identical bytes");
        assert_eq!(rehydrated.id, object_id);
        assert_eq!(
            library.read_object(object_id).expect("read rehydrated"),
            payload
        );
        assert_eq!(
            library
                .purge_authorized_absence(object_id)
                .expect("active absence lookup"),
            None
        );
        let historical = library
            .purge_authorized_absence_for_purge(receipt.purge_id, object_id)
            .expect("historical absence lookup")
            .expect("historical absence");
        assert!(historical.superseded_at.is_some());
        assert_eq!(
            library
                .verify_purge_cleanup_state(receipt.purge_id)
                .expect("verify succeeded superseded cleanup")
                .state,
            PurgeJournalState::Succeeded
        );
    }
}
