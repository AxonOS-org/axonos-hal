//! The seam as the rest of the system uses it: close a budget, configure a
//! device, run a session, and account for every frame the device produced.

use axonos_hal::{
    sim::{FaultProfile, SimDevice},
    AcqError, AcquisitionDevice, Frontend, SampleFrame, TimingBudget,
};

/// The property the whole crate exists to guarantee: delivered frames plus
/// reported losses equal frames produced. Nothing vanishes quietly.
#[test]
fn every_produced_frame_is_either_delivered_or_reported_lost() {
    let budget = TimingBudget::canonical(250).expect("canonical rate must close");
    let mut dev = SimDevice::new(7, FaultProfile::FIELD);
    dev.configure(budget, Frontend::CANONICAL).unwrap();

    let mut delivered: Vec<SampleFrame> = Vec::new();
    let mut reported_lost = 0u64;
    let mut integrity_failures = 0u64;

    for _ in 0..5_000 {
        match dev.read_frame() {
            Ok(f) => delivered.push(f),
            Err(AcqError::Overrun { lost }) => reported_lost += lost as u64,
            Err(AcqError::Integrity) => integrity_failures += 1,
            Err(e) => panic!("session should not have died: {e:?}"),
        }
    }

    // Independently reconstruct the loss from the sequence numbers alone,
    // without consulting the diagnostics — if these two disagree, one of them
    // is lying and the stream cannot be trusted either way.
    let mut gaps = 0u64;
    for w in delivered.windows(2) {
        gaps += w[1].gap_since(&w[0]).expect("sequence must never rewind") as u64;
    }

    let diag = dev.diagnostics();
    assert_eq!(diag.frames, delivered.len() as u64);
    assert_eq!(diag.frames_lost, reported_lost);
    assert_eq!(
        gaps, reported_lost,
        "sequence numbers and the overrun counter must tell the same story"
    );
    assert_eq!(diag.integrity_failures as u64, integrity_failures);
    assert!(
        diag.integrity_ppm() > 990_000,
        "field profile should stay usable"
    );
}

#[test]
fn a_device_cannot_be_started_into_an_unmeetable_deadline() {
    // 2 kSPS cannot fit the canonical chain, so no budget exists to configure
    // with — the refusal happens before a device is touched, by construction.
    assert!(TimingBudget::canonical(2_000).is_err());

    let mut dev = SimDevice::new(1, FaultProfile::CLEAN);
    assert_eq!(dev.read_frame(), Err(AcqError::NotConfigured));
}

#[test]
fn a_session_is_reproducible_frame_for_frame() {
    let budget = TimingBudget::canonical(500).unwrap();
    let run = |seed: u64| {
        let mut d = SimDevice::new(seed, FaultProfile::FIELD);
        d.configure(budget, Frontend::CANONICAL).unwrap();
        (0..2_000).map(|_| d.read_frame()).collect::<Vec<_>>()
    };
    assert_eq!(run(99), run(99));
    assert_ne!(run(99), run(100));
}

#[test]
fn samples_convert_to_a_plausible_biosignal_range() {
    let budget = TimingBudget::canonical(250).unwrap();
    let mut dev = SimDevice::new(3, FaultProfile::CLEAN);
    dev.configure(budget, Frontend::CANONICAL).unwrap();
    let fe = Frontend::CANONICAL;
    for _ in 0..1_000 {
        let f = dev.read_frame().unwrap();
        for ch in 0..8 {
            let nv = f.nanovolts(ch, fe);
            // tens of microvolts, i.e. tens of thousands of nanovolts
            assert!(nv.abs() < 100_000, "{nv} nV is not an EEG amplitude");
        }
    }
}
