//! Simulated file transfer for firmware downloads and log uploads - `docs/hardware-roadmap.md`'s
//! H10, the piece [`super::firmware`]'s traits can't cover alone: moving bytes to and from
//! wherever OCPP's URL points.
//!
//! [`FakeFileTransfer`] paces both `download` and `upload` over injected simulated time exactly
//! like [`super::firmware::FakeFirmwareInstaller`] - see [`Self::tick`] - and reports
//! `TransferProgress` (`ocpp_charge_point::hardware::TransferProgress`) as it goes, synthesized
//! from however far its configured [`TransferProfile`] duration has ticked. `download` and
//! `upload` are independently configurable and independently failable
//! ([`Self::trigger_download_failure`]/[`Self::trigger_upload_failure`]) - a CSMS developer needs
//! to drive a log upload to completion *and* to failure, deliberately and reproducibly, without
//! that also touching a firmware download in flight on the same charger.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ocpp_charge_point::hardware::{FileTransfer, TransferProgress, UploadSource};
use tokio::sync::watch;

/// How long a simulated transfer takes and how large it claims to be - the two knobs
/// [`FakeFileTransfer::new`] needs for `download` and, independently, `upload`.
#[derive(Debug, Clone, Copy)]
pub struct TransferProfile {
    /// Simulated time the transfer takes to complete, paced by [`FakeFileTransfer::tick`].
    /// [`Duration::ZERO`] completes on the first check, with no tick required - useful for a
    /// scenario that only cares about the eventual `Ok`/`Err`, not the pacing.
    pub duration: Duration,
    /// The total size this transfer reports via `TransferReport::total_bytes`
    /// (`ocpp_charge_point::hardware::TransferReport`) - purely cosmetic, since no real bytes
    /// exist on either side of this fake (see [`FileTransfer`]'s own module docs for why the crate
    /// never sees transfer content).
    pub total_bytes: u64,
}

impl TransferProfile {
    /// A transfer profile that completes instantly and reports `total_bytes` as its size.
    pub fn instant(total_bytes: u64) -> Self {
        Self {
            duration: Duration::ZERO,
            total_bytes,
        }
    }
}

/// A transfer that is running right now, as an outside observer can see it - returned by
/// [`FakeFileTransfer::download_in_flight`]/[`FakeFileTransfer::upload_in_flight`].
///
/// Purely observational, exactly like [`super::firmware::FirmwareInstallStage`]: nothing in
/// `ocpp-charge-point` reads it, and reading it neither advances nor disturbs the transfer. It
/// exists because the progress a transfer *reports* goes to upstream's `TransferProgress` callback
/// (and from there to the CSMS as `FirmwareStatusNotification`/`LogStatusNotification`), which a
/// frontend cannot intercept - so without this, a firmware download in flight is invisible to
/// anything but the CSMS on the other end.
///
/// There is deliberately no completed/failed variant: the slot this is read from exists only while
/// a transfer is running, so `None` means "nothing in flight" and cannot distinguish "finished" from
/// "never started". A caller wanting the outcome has it already - `download`/`upload` return it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlightTransfer {
    /// Simulated time ticked into this transfer so far.
    pub elapsed: Duration,
    /// The configured total from this half's [`TransferProfile`], for turning `elapsed` into a
    /// fraction. Never zero for a paced transfer; a [`TransferProfile::instant`] one completes
    /// before anything could observe it.
    pub duration: Duration,
    /// How many of `total_bytes` have notionally moved - the same figure reported to upstream.
    pub transferred_bytes: u64,
    /// The size this transfer claims, from its [`TransferProfile`].
    pub total_bytes: u64,
}

impl InFlightTransfer {
    /// Completed fraction, `0.0..=1.0`, from the byte counts rather than the durations so it can
    /// never disagree with the figure the CSMS was told.
    pub fn fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            return 1.0;
        }
        (self.transferred_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0)
    }
}

/// A simulated file transfer: no network, no bytes, just [`Self::tick`]-paced progress and a
/// configurable, independently-failable outcome for each of `download` and `upload`.
#[derive(Debug)]
pub struct FakeFileTransfer {
    download: TransferProfile,
    upload: TransferProfile,
    /// The in-flight download's progress channel, mirroring
    /// [`super::firmware::FakeFirmwareInstaller`]'s `progress` field - `None` whenever no
    /// [`FileTransfer::download`] call is currently awaiting one.
    download_progress: Mutex<Option<watch::Sender<Duration>>>,
    /// Same as `download_progress`, for [`FileTransfer::upload`].
    upload_progress: Mutex<Option<watch::Sender<Duration>>>,
    download_fail: AtomicBool,
    upload_fail: AtomicBool,
    /// The most recent [`UploadSource::Bytes`] this transfer was asked to upload, if any - `None`
    /// both before the first upload and whenever the most recent one was
    /// [`UploadSource::Local`] (this fake never sees real bytes for that variant, per this
    /// module's own docs). Exists purely for tests: unlike `download`/`upload`'s pacing, which is
    /// observable through [`Self::tick`]/`TransferProgress`, nothing else about `upload`'s content
    /// is otherwise reachable from outside this type, and `docs/hardware-roadmap.md`'s decision 8
    /// (`log_uploads` sharing the restored `SecurityEventLog`) needs to verify what actually got
    /// rendered and handed to `upload`, not merely that it was called.
    last_upload_bytes: Mutex<Option<Vec<u8>>>,
}

impl FakeFileTransfer {
    /// A transfer using `download`'s profile for [`FileTransfer::download`] and `upload`'s for
    /// [`FileTransfer::upload`].
    pub fn new(download: TransferProfile, upload: TransferProfile) -> Self {
        Self {
            download,
            upload,
            download_progress: Mutex::new(None),
            upload_progress: Mutex::new(None),
            download_fail: AtomicBool::new(false),
            upload_fail: AtomicBool::new(false),
            last_upload_bytes: Mutex::new(None),
        }
    }

    /// How far the in-flight [`FileTransfer::download`] has got, or `None` when no download is
    /// currently running - see [`InFlightTransfer`].
    pub fn download_in_flight(&self) -> Option<InFlightTransfer> {
        Self::in_flight(&self.download, &self.download_progress)
    }

    /// How far the in-flight [`FileTransfer::upload`] has got, or `None` when no upload is
    /// currently running - see [`InFlightTransfer`].
    pub fn upload_in_flight(&self) -> Option<InFlightTransfer> {
        Self::in_flight(&self.upload, &self.upload_progress)
    }

    /// Reads a half's progress channel without disturbing it. The slot is `Some` for exactly as long
    /// as [`Self::run`]'s loop is running (it sets it on entry and clears it on the way out,
    /// whichever way that goes), which is what makes its presence the honest answer to "is a
    /// transfer in flight right now?".
    fn in_flight(
        profile: &TransferProfile,
        slot: &Mutex<Option<watch::Sender<Duration>>>,
    ) -> Option<InFlightTransfer> {
        let sender = slot.lock().expect("lock poisoned").clone()?;
        let elapsed = *sender.borrow();
        Some(InFlightTransfer {
            elapsed,
            duration: profile.duration,
            transferred_bytes: transferred_bytes(profile, elapsed),
            total_bytes: profile.total_bytes,
        })
    }

    /// The bytes handed to the most recent [`FileTransfer::upload`] call, if it carried
    /// [`UploadSource::Bytes`] - see [`Self::last_upload_bytes`]'s field doc comment.
    pub fn last_upload(&self) -> Option<Vec<u8>> {
        self.last_upload_bytes
            .lock()
            .expect("lock poisoned")
            .clone()
    }

    /// Advances whichever of `download`/`upload` is currently in flight by `elapsed` - both, if
    /// both happen to be (a firmware download and a diagnostics upload can be independent
    /// campaigns in flight on the same charger at once). A no-op for a half with nothing awaiting
    /// it, the same "nothing to drive yet" stance
    /// [`super::firmware::FakeFirmwareInstaller::tick`] takes.
    pub fn tick(&self, elapsed: Duration) {
        Self::advance(&self.download_progress, elapsed);
        Self::advance(&self.upload_progress, elapsed);
    }

    fn advance(slot: &Mutex<Option<watch::Sender<Duration>>>, elapsed: Duration) {
        if let Some(sender) = slot.lock().expect("lock poisoned").clone() {
            let total = *sender.borrow() + elapsed;
            let _ = sender.send(total);
        }
    }

    fn wake(slot: &Mutex<Option<watch::Sender<Duration>>>) {
        if let Some(sender) = slot.lock().expect("lock poisoned").clone() {
            let current = *sender.borrow();
            let _ = sender.send(current);
        }
    }

    /// Makes the current (or next) [`FileTransfer::download`] call fail with
    /// [`FakeFileTransferError`] - the deliberate, reproducible way to exercise a CSMS's
    /// `DownloadFailed` handling. Wakes a download already waiting on [`Self::tick`] so the
    /// failure surfaces immediately.
    pub fn trigger_download_failure(&self) {
        self.download_fail.store(true, Ordering::Relaxed);
        Self::wake(&self.download_progress);
    }

    /// Makes the current (or next) [`FileTransfer::upload`] call fail with
    /// [`FakeFileTransferError`] - the deliberate, reproducible way to exercise a CSMS's
    /// `UploadFailure` handling (a log upload, most commonly).
    pub fn trigger_upload_failure(&self) {
        self.upload_fail.store(true, Ordering::Relaxed);
        Self::wake(&self.upload_progress);
    }

    async fn run(
        profile: &TransferProfile,
        slot: &Mutex<Option<watch::Sender<Duration>>>,
        fail: &AtomicBool,
        progress: &TransferProgress<'_>,
    ) -> Result<(), FakeFileTransferError> {
        let (sender, mut receiver) = watch::channel(Duration::ZERO);
        *slot.lock().expect("lock poisoned") = Some(sender);

        let result = loop {
            let elapsed = *receiver.borrow();
            progress.report(
                transferred_bytes(profile, elapsed),
                Some(profile.total_bytes),
            );

            if fail.load(Ordering::Relaxed) {
                break Err(FakeFileTransferError);
            }
            if elapsed >= profile.duration {
                break Ok(());
            }
            if receiver.changed().await.is_err() {
                // Unreachable while this loop is running - see the matching comment in
                // `FakeFirmwareInstaller::install`.
                break Err(FakeFileTransferError);
            }
        };

        *slot.lock().expect("lock poisoned") = None;
        result
    }
}

/// How many of `profile.total_bytes` a transfer `elapsed` simulated time into `profile.duration`
/// has notionally moved, clamped to the total - the inverse of the percentage math
/// `TransferReport::percent` (`ocpp_charge_point::hardware::TransferReport`) applies on the
/// receiving end, run here to synthesize a report in the first place.
fn transferred_bytes(profile: &TransferProfile, elapsed: Duration) -> u64 {
    if profile.duration.is_zero() {
        return profile.total_bytes;
    }
    let fraction = (elapsed.as_secs_f64() / profile.duration.as_secs_f64()).min(1.0);
    (profile.total_bytes as f64 * fraction).round() as u64
}

/// The error [`FakeFileTransfer::download`]/[`FakeFileTransfer::upload`] returns after
/// [`FakeFileTransfer::trigger_download_failure`]/[`FakeFileTransfer::trigger_upload_failure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FakeFileTransferError;

impl std::fmt::Display for FakeFileTransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("simulated file transfer failed")
    }
}

impl std::error::Error for FakeFileTransferError {}

#[async_trait::async_trait]
impl FileTransfer for FakeFileTransfer {
    type Error = FakeFileTransferError;

    async fn download(
        &self,
        url: &str,
        progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error> {
        tracing::info!(url, "file download starting");
        let result = Self::run(
            &self.download,
            &self.download_progress,
            &self.download_fail,
            progress,
        )
        .await;
        match &result {
            Ok(()) => tracing::info!(url, "file download completed"),
            Err(_) => tracing::warn!(url, "file download failed"),
        }
        result
    }

    async fn upload(
        &self,
        url: &str,
        source: UploadSource<'_>,
        progress: &TransferProgress<'_>,
    ) -> Result<(), Self::Error> {
        tracing::info!(url, ?source, "file upload starting");
        // Captured for tests - see `Self::last_upload_bytes`'s field doc comment. `Local` carries
        // no bytes this fake ever sees, so it leaves the previous capture untouched rather than
        // clearing it to `None` - a caller reading `last_upload` after a `Local` upload should see
        // "nothing changed", not "nothing was ever uploaded".
        if let UploadSource::Bytes(bytes) = &source {
            *self.last_upload_bytes.lock().expect("lock poisoned") = Some(bytes.to_vec());
        }
        let result = Self::run(
            &self.upload,
            &self.upload_progress,
            &self.upload_fail,
            progress,
        )
        .await;
        match &result {
            Ok(()) => tracing::info!(url, "file upload completed"),
            Err(_) => tracing::warn!(url, "file upload failed"),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocpp_charge_point::hardware::{LogKind, TransferReport};

    #[tokio::test]
    async fn download_progresses_through_simulated_time_and_completes() {
        let transfer = FakeFileTransfer::new(
            TransferProfile {
                duration: Duration::from_secs(10),
                total_bytes: 1000,
            },
            TransferProfile::instant(0),
        );

        let seen: Mutex<Vec<u8>> = Mutex::new(Vec::new());
        let record = |report: TransferReport| {
            if let Some(percent) = report.percent() {
                seen.lock().expect("lock poisoned").push(percent);
            }
        };
        let progress = TransferProgress::new(&record);

        let download = transfer.download("https://example.invalid/fw.bin", &progress);
        let drive = async {
            tokio::task::yield_now().await;
            transfer.tick(Duration::from_secs(5));
            tokio::task::yield_now().await;
            transfer.tick(Duration::from_secs(5));
        };

        let (result, ()) = tokio::join!(download, drive);
        result.unwrap();

        let seen = seen.into_inner().expect("lock poisoned");
        assert_eq!(
            seen.first(),
            Some(&0),
            "first report should show no progress yet"
        );
        assert_eq!(
            seen.last(),
            Some(&100),
            "final report should show completion"
        );
        assert!(
            seen.windows(2).all(|pair| pair[0] <= pair[1]),
            "progress must never go backwards: {seen:?}"
        );
    }

    /// The observational accessor a frontend needs: nothing else can see a transfer's progress,
    /// since the reports themselves go to upstream's callback and on to the CSMS.
    #[tokio::test]
    async fn an_in_flight_download_is_observable_and_stops_being_so_once_it_finishes() {
        let transfer = FakeFileTransfer::new(
            TransferProfile {
                duration: Duration::from_secs(10),
                total_bytes: 1000,
            },
            TransferProfile::instant(0),
        );

        assert_eq!(
            transfer.download_in_flight(),
            None,
            "nothing is in flight before `download` is even called"
        );

        let ignored = TransferProgress::ignored();
        let download = transfer.download("https://example.invalid/fw.bin", &ignored);
        let observe = async {
            tokio::task::yield_now().await;
            let started = transfer
                .download_in_flight()
                .expect("a download awaiting ticks is in flight");
            assert_eq!(started.transferred_bytes, 0);
            assert_eq!(started.total_bytes, 1000);
            assert_eq!(started.fraction(), 0.0);

            transfer.tick(Duration::from_secs(4));
            tokio::task::yield_now().await;
            let midway = transfer
                .download_in_flight()
                .expect("still in flight, four of ten seconds in");
            assert_eq!(midway.transferred_bytes, 400);
            assert!((midway.fraction() - 0.4).abs() < 1e-9, "{midway:?}");
            // An upload nobody started must not pick up the download's progress.
            assert_eq!(transfer.upload_in_flight(), None);

            transfer.tick(Duration::from_secs(6));
        };

        let (result, ()) = tokio::join!(download, observe);
        result.unwrap();

        assert_eq!(
            transfer.download_in_flight(),
            None,
            "a finished download is no longer in flight"
        );
    }

    #[tokio::test]
    async fn splitting_the_same_elapsed_time_across_many_small_ticks_matches_one_large_tick() {
        async fn run_download(
            transfer: &FakeFileTransfer,
            chunks: &[Duration],
        ) -> Result<(), FakeFileTransferError> {
            let ignored = TransferProgress::ignored();
            let download = transfer.download("https://example.invalid/fw.bin", &ignored);
            let drive = async {
                tokio::task::yield_now().await;
                for &chunk in chunks {
                    transfer.tick(chunk);
                }
            };
            let (result, ()) = tokio::join!(download, drive);
            result
        }

        let profile = TransferProfile {
            duration: Duration::from_secs(10),
            total_bytes: 500,
        };

        let one_tick = FakeFileTransfer::new(profile, TransferProfile::instant(0));
        run_download(&one_tick, &[Duration::from_secs(10)])
            .await
            .unwrap();

        let many_ticks = FakeFileTransfer::new(profile, TransferProfile::instant(0));
        let chunks = vec![Duration::from_millis(100); 100];
        run_download(&many_ticks, &chunks).await.unwrap();
    }

    #[tokio::test]
    async fn trigger_download_failure_surfaces_as_err_without_panicking() {
        let transfer = FakeFileTransfer::new(
            TransferProfile {
                duration: Duration::from_secs(10),
                total_bytes: 1000,
            },
            TransferProfile::instant(0),
        );

        let ignored = TransferProgress::ignored();
        let download = transfer.download("https://example.invalid/fw.bin", &ignored);
        let drive = async {
            tokio::task::yield_now().await;
            transfer.trigger_download_failure();
        };
        let (result, ()) = tokio::join!(download, drive);

        assert_eq!(result, Err(FakeFileTransferError));

        // A failed download must not leave the channel behind - ticking afterwards is harmless.
        transfer.tick(Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_log_upload_progresses_and_completes() {
        let transfer = FakeFileTransfer::new(
            TransferProfile::instant(0),
            TransferProfile {
                duration: Duration::from_secs(4),
                total_bytes: 2_000,
            },
        );

        let seen: Mutex<Vec<u8>> = Mutex::new(Vec::new());
        let record = |report: TransferReport| {
            if let Some(percent) = report.percent() {
                seen.lock().expect("lock poisoned").push(percent);
            }
        };
        let progress = TransferProgress::new(&record);

        let upload = transfer.upload(
            "https://example.invalid/logs",
            UploadSource::Local(LogKind::Diagnostics),
            &progress,
        );
        let drive = async {
            tokio::task::yield_now().await;
            transfer.tick(Duration::from_secs(2));
            tokio::task::yield_now().await;
            transfer.tick(Duration::from_secs(2));
        };

        let (result, ()) = tokio::join!(upload, drive);
        result.unwrap();

        let seen = seen.into_inner().expect("lock poisoned");
        assert_eq!(seen.last(), Some(&100));
    }

    #[tokio::test]
    async fn an_upload_failure_is_reportable() {
        let transfer = FakeFileTransfer::new(
            TransferProfile::instant(0),
            TransferProfile {
                duration: Duration::from_secs(10),
                total_bytes: 500,
            },
        );

        let ignored = TransferProgress::ignored();
        let upload = transfer.upload(
            "https://example.invalid/logs",
            UploadSource::Bytes(b"security log"),
            &ignored,
        );
        let drive = async {
            tokio::task::yield_now().await;
            transfer.trigger_upload_failure();
        };
        let (result, ()) = tokio::join!(upload, drive);

        assert_eq!(result, Err(FakeFileTransferError));
    }

    #[tokio::test]
    async fn download_and_upload_progress_independently_when_both_are_in_flight() {
        let transfer = FakeFileTransfer::new(
            TransferProfile {
                duration: Duration::from_secs(10),
                total_bytes: 100,
            },
            TransferProfile {
                duration: Duration::from_secs(10),
                total_bytes: 100,
            },
        );

        let download_ignored = TransferProgress::ignored();
        let download = transfer.download("https://example.invalid/fw.bin", &download_ignored);
        let upload_ignored = TransferProgress::ignored();
        let upload = transfer.upload(
            "https://example.invalid/logs",
            UploadSource::Local(LogKind::Diagnostics),
            &upload_ignored,
        );
        let drive = async {
            // Two yields: one for each of `download`/`upload` to register its progress channel.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            transfer.tick(Duration::from_secs(10));
        };

        let (download_result, upload_result, ()) = tokio::join!(download, upload, drive);

        assert!(download_result.is_ok());
        assert!(upload_result.is_ok());
    }

    #[tokio::test]
    async fn an_instant_transfer_completes_without_any_tick() {
        let transfer =
            FakeFileTransfer::new(TransferProfile::instant(42), TransferProfile::instant(7));

        let downloaded = transfer
            .download(
                "https://example.invalid/fw.bin",
                &TransferProgress::ignored(),
            )
            .await;
        let uploaded = transfer
            .upload(
                "https://example.invalid/logs",
                UploadSource::Local(LogKind::Diagnostics),
                &TransferProgress::ignored(),
            )
            .await;

        assert!(downloaded.is_ok());
        assert!(uploaded.is_ok());
    }
}
