//! The device as the sidebar knows it, kept by a worker thread so the window never waits.
//!
//! Every action goes through the OpenDAP CLI's crates, the same use cases `opendap` and OpenDAP
//! Desktop run (opendap-core over opendap-transport-vm): attaching by hand (`hardware`), the
//! battery, restarts and power-off (`power`), the display (`screen`). The state machines that
//! lock a control while its action is in flight come from there too: a switch stays attaching
//! until the hardware has the card or the disk ([`Switch`]), a reboot lasts until the device
//! went down and answers again ([`Phase`]), so nothing is sent twice or undone half-way.
//!
//! The state is the virtual hardware's (a card in its socket), never what the device's system
//! makes of it (a card mounted): the simulator simulates the physical side.

use std::{
    path::PathBuf,
    sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    thread,
    time::{Duration, Instant},
};

use opendap_core::hardware::{self, Switch};
use opendap_core::power::{self, Phase};
use opendap_core::{Outcome, VmBackend};
use opendap_model::{Attachment, HardwareChange, PowerAction, Transport, VmState};
use opendap_transport_ssh::SshExec;
use opendap_transport_vm::{Checkout, SSH_LAUNCHER, VirtualDevice};

/// What the window shows.
#[derive(Clone, Debug, Default)]
pub struct State {
    pub phase: Phase,
    /// One switch per [`Attachment::ALL`].
    pub switches: [Switch; 4],
    /// The debug UI mode, a state of the device's software, read once it answers.
    pub debug_ui: Switch,
    /// The battery's charge, when the board's battery model answers.
    pub battery: Option<u8>,
    /// A battery change is in flight.
    pub battery_busy: bool,
    /// The reboot in flight goes into the debug mode.
    pub debug_reboot: bool,
    /// A display capture is in flight.
    pub saving: bool,
    /// The last thing worth saying (an error, a saved file), for a few seconds.
    pub note: String,
    pub note_since: Option<Instant>,
}

impl State {
    pub fn switch(&self, a: Attachment) -> Switch {
        self.switches[index(a)]
    }

    /// The window's code for a switch: 0 off, 1 attaching, 2 on, 3 detaching.
    pub fn code(&self, a: Attachment) -> i32 {
        match self.switch(a) {
            Switch::Off => 0,
            Switch::Attaching => 1,
            Switch::On => 2,
            Switch::Detaching => 3,
        }
    }
}

fn index(a: Attachment) -> usize {
    Attachment::ALL.iter().position(|x| *x == a).unwrap()
}

pub enum Job {
    Attach(Attachment, bool),
    Battery(u8),
    SavePng(PathBuf),
    DebugUi(bool),
    Power(PowerAction),
}

/// How long the worker waits for the hardware or the device before it gives up on a change.
const ATTACH_WAIT: Duration = Duration::from_secs(20);
const REBOOT_WAIT: Duration = Duration::from_secs(240);
const STOP_WAIT: Duration = Duration::from_secs(60);
/// How often the device is looked at: often while something is in flight, rarely otherwise
/// (each look runs `odap-vdev`).
const POLL_BUSY: Duration = Duration::from_millis(700);
const POLL_IDLE: Duration = Duration::from_secs(4);
/// How long a note stays.
const NOTE_FOR: Duration = Duration::from_secs(6);

pub struct Worker {
    pub jobs: Sender<Job>,
}

impl Worker {
    /// Starts the worker for the device of the checkout at `vd_dir`; `publish` gets every new
    /// state, on the worker's thread.
    pub fn start(vd_dir: PathBuf, publish: impl Fn(State) + Send + 'static) -> Self {
        let (tx, rx) = channel();
        thread::spawn(move || Run::new(vd_dir).run(rx, publish));
        Worker { jobs: tx }
    }
}

struct Run {
    vd_dir: PathBuf,
    vm: VirtualDevice,
    target: Transport,
    state: State,
    /// When the power request in flight is given up.
    power_deadline: Option<Instant>,
    /// When each attachment in flight is given up.
    attach_deadline: [Option<Instant>; 4],
}

impl Run {
    fn new(vd_dir: PathBuf) -> Self {
        Run {
            vm: VirtualDevice::at(vd_dir.clone()),
            vd_dir,
            target: Transport::Vm {
                name: opendap_model::vm::DEFAULT_NAME.into(),
            },
            state: State::default(),
            power_deadline: None,
            attach_deadline: [None; 4],
        }
    }

    fn note(&mut self, text: impl Into<String>) {
        self.state.note = text.into();
        self.state.note_since = Some(Instant::now());
    }

    /// `odap-vdev ui debug [on|off]`: the mode after the change, if any.
    fn debug_ui(&self, set: Option<bool>) -> Option<bool> {
        let mut cmd = std::process::Command::new(self.vm.tool());
        cmd.args(["ui", "debug"]);
        if let Some(on) = set {
            cmd.arg(if on { "on" } else { "off" });
        }
        let out = cmd.output().ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim() == "on")
    }

    fn busy(&self) -> bool {
        self.state.phase.busy() && self.state.phase != Phase::Off
            || self.state.switches.iter().any(|s| s.busy())
    }

    /// Look at the device and the hardware, and move the state machines.
    fn refresh(&mut self) {
        if self.state.note_since.is_some_and(|t| t.elapsed() > NOTE_FOR) {
            self.state.note.clear();
            self.state.note_since = None;
        }
        let st = self.vm.state();
        let (running, answering) = (st != VmState::Stopped, st == VmState::Running);
        let was_ready = self.state.phase == Phase::Ready;
        self.state.phase.observe(running, answering);
        // The debug UI mode, read when the device starts answering (a reboot resets it).
        if answering && !was_ready && self.state.phase == Phase::Ready {
            if let Some(on) = self.debug_ui(None) {
                self.state.debug_ui.observe(on);
            }
        }
        if let Some(by) = self.power_deadline {
            if !matches!(self.state.phase, Phase::Rebooting { .. } | Phase::Stopping) {
                self.power_deadline = None;
            } else if Instant::now() > by {
                self.power_deadline = None;
                self.state.phase.abandon(running, answering);
                self.note("the device did not come back in time");
            }
        }
        if !running {
            return;
        }
        match hardware::state(&self.vm, &self.target) {
            Ok(hw) => {
                for a in Attachment::ALL {
                    let i = index(a);
                    let has = hw.has(a);
                    let s = &mut self.state.switches[i];
                    s.observe(has);
                    if !s.busy() {
                        self.attach_deadline[i] = None;
                    } else if self.attach_deadline[i].is_some_and(|by| Instant::now() > by) {
                        s.failed(has);
                        self.attach_deadline[i] = None;
                        self.note(format!("the hardware did not take the {a} change"));
                    }
                }
                // What the hardware reports, always: the window's readout follows it.
                self.state.battery = hw.battery;
            }
            Err(e) => self.note(e.message),
        }
    }

    fn run(mut self, rx: Receiver<Job>, publish: impl Fn(State)) {
        self.refresh();
        publish(self.state.clone());
        loop {
            let wait = if self.busy() { POLL_BUSY } else { POLL_IDLE };
            match rx.recv_timeout(wait) {
                Ok(job) => {
                    // Whatever else is queued was asked with the state the window had then; the
                    // state machines drop what no longer applies. Values sent fire-and-forget (the
                    // battery) keep only the latest.
                    let mut queue: Vec<Job> = std::iter::once(job).chain(rx.try_iter()).collect();
                    if let Some(last) = queue.iter().rposition(|j| matches!(j, Job::Battery(_))) {
                        let mut i = 0;
                        queue.retain(|j| {
                            let keep = !matches!(j, Job::Battery(_)) || i == last;
                            i += 1;
                            keep
                        });
                    }
                    for job in queue {
                        self.job(job, &publish);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.refresh();
            publish(self.state.clone());
        }
    }

    fn job(&mut self, job: Job, publish: &impl Fn(State)) {
        match job {
            Job::Attach(a, on) => {
                let i = index(a);
                // Debounce: a switch in flight, or already where it is asked to go, takes nothing.
                let Some(kind) = self.state.switches[i].request(on) else {
                    return;
                };
                self.attach_deadline[i] = Some(Instant::now() + ATTACH_WAIT);
                publish(self.state.clone());
                match hardware::change(&self.vm, self.target.clone(), kind.of(a), false) {
                    Ok(Outcome::Done(done)) => self.state.switches[i].observe(done.state.has(a)),
                    Ok(Outcome::Plan(_)) => {}
                    Err(e) => {
                        // Nothing changed: the hardware still is as it was before the request.
                        self.state.switches[i].failed(!on);
                        self.attach_deadline[i] = None;
                        self.note(e.message);
                    }
                }
            }
            Job::Battery(percent) => {
                self.state.battery_busy = true;
                let change = HardwareChange::Battery { percent };
                if let Err(e) = hardware::change(&self.vm, self.target.clone(), change, false) {
                    self.note(e.message);
                }
                self.state.battery_busy = false;
            }
            Job::SavePng(path) => {
                if self.state.saving {
                    return;
                }
                self.state.saving = true;
                publish(self.state.clone());
                let p = path.display().to_string();
                let note =
                    match opendap_core::screen::capture(&self.vm, self.target.clone(), &p, false) {
                        Ok(_) => format!(
                            "Saved {}",
                            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(p)
                        ),
                        Err(e) => e.message,
                    };
                self.note(note);
                self.state.saving = false;
            }
            Job::DebugUi(on) => {
                if self.state.debug_ui.request(on).is_none() {
                    return;
                }
                publish(self.state.clone());
                match self.debug_ui(Some(on)) {
                    Some(now) => self.state.debug_ui.observe(now),
                    None => {
                        self.state.debug_ui.failed(!on);
                        self.note("the debug UI mode did not change");
                    }
                }
            }
            Job::Power(action) => {
                // Only a ready device takes one, and only once.
                if !self.state.phase.request(action) {
                    return;
                }
                self.state.note.clear();
                self.state.debug_reboot = action
                    == PowerAction::Reboot {
                        mode: opendap_model::BootMode::Debug,
                    };
                self.power_deadline = Some(
                    Instant::now()
                        + if action == PowerAction::Poweroff {
                            STOP_WAIT
                        } else {
                            REBOOT_WAIT
                        },
                );
                publish(self.state.clone());
                let done =
                    power::power(&Checkout(self.vd_dir.clone()), None, action, false, |vm| {
                        SshExec::new(move || vm.ssh_command(), SSH_LAUNCHER)
                    });
                if let Err(e) = done {
                    self.note(e.message);
                    self.power_deadline = None;
                    let st = self.vm.state();
                    self.state
                        .phase
                        .abandon(st != VmState::Stopped, st == VmState::Running);
                }
            }
        }
    }
}
