use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use crate::cgroup;

const CGROUP_BASE: &str = "/sys/fs/cgroup";

/// Bias systemd-oomd against killing a protected cgroup. This is the crucial
/// companion to `memory.min`: memory.min only fends off *kernel* page reclaim,
/// but systemd-oomd's pressure-triggered SIGKILL ignores it entirely — which is
/// how oomd tore down the compositor cgroup out from under us mid-mitigation.
/// The `user.oomd_avoid` xattr is oomd's ManagedOOMPreference=avoid: oomd will
/// pick such a cgroup only as a last resort, after everything else. Best-effort
/// — silently a no-op on kernels/filesystems that don't take the xattr.
fn set_oomd_avoid(cgroup_path: &Path, avoid: bool) {
    let Ok(cpath) = CString::new(cgroup_path.as_os_str().as_bytes()) else {
        return;
    };
    let name = c"user.oomd_avoid";
    unsafe {
        if avoid {
            let val = b"1";
            let _ = nix::libc::setxattr(
                cpath.as_ptr(),
                name.as_ptr(),
                val.as_ptr() as *const nix::libc::c_void,
                val.len(),
                0,
            );
        } else {
            let _ = nix::libc::removexattr(cpath.as_ptr(), name.as_ptr());
        }
    }
}

// Single source of truth for every memory.min protection we set — compositor,
// audio, HUD pin, and foreground alike — keyed by the protected leaf cgroup.
// Ancestor values are *derived* from this, so a protection can be released in
// full, including the residue it would otherwise leave on shared parent slices.
static PROTECTIONS: LazyLock<Mutex<HashMap<PathBuf, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// Every cgroup we've ever written memory.min on, so a recompute can zero the
// ones it no longer needs (rather than leaking them until reboot).
static TOUCHED: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Pure core: given the active protections, the memory.min each cgroup should
/// carry — every protected leaf and its ancestors (up to, but excluding, the
/// cgroup root) take the *max* protection passing through them, because the
/// kernel needs a parent's memory.min >= its protected child's. Testable without
/// touching cgroupfs.
fn desired_min_map(protections: &HashMap<PathBuf, u64>, base: &Path) -> HashMap<PathBuf, u64> {
    let mut desired: HashMap<PathBuf, u64> = HashMap::new();
    for (leaf, &val) in protections {
        let mut cur: Option<&Path> = Some(leaf.as_path());
        while let Some(cg) = cur {
            if cg == base || !cg.starts_with(base) {
                break;
            }
            let e = desired.entry(cg.to_path_buf()).or_insert(0);
            if val > *e {
                *e = val;
            }
            cur = cg.parent();
        }
    }
    desired
}

/// Recompute memory.min across the whole tree from the registry and write it,
/// zeroing any cgroup a prior recompute touched that's no longer needed. This is
/// what makes protections fully reversible.
fn apply() {
    let base = Path::new(CGROUP_BASE);
    let desired = {
        let mut prot = PROTECTIONS.lock().unwrap_or_else(|e| e.into_inner());
        // Self-heal: a protected cgroup that's gone (e.g. a prior HUD process that
        // exited) is dropped, so the registry can't grow without bound and its
        // ancestor residue is released.
        prot.retain(|leaf, _| leaf.exists());
        desired_min_map(&prot, base)
    };
    let mut touched = TOUCHED.lock().unwrap_or_else(|e| e.into_inner());
    for (cg, val) in &desired {
        let _ = cgroup::write_cgroup(cg, "memory.min", &val.to_string());
    }
    for cg in touched.iter() {
        if !desired.contains_key(cg) {
            let _ = cgroup::write_cgroup(cg, "memory.min", "0");
        }
    }
    *touched = desired.keys().cloned().collect();
}

/// System page size. The kernel stores memory.min rounded DOWN to a multiple of
/// this, so we align our targets to it — otherwise the read-back verification in
/// set_protection reads a value a few KB below what we asked for and every
/// protection falsely reports failure.
/// Is this cgroup small enough to hold wholly in RAM? Pinning something out of
/// swap reserves its entire footprint, so the lever only makes sense for things
/// whose whole point is instant response and whose size is bounded: the session
/// spine and the focused terminal. Ceiling is 1/8 of RAM (~1.9GB on a 15GB box) —
/// comfortably above a compositor (~1GB) or a terminal (~250MB), and safely below
/// a browser.
fn swap_pin_eligible(cgroup_path: &Path) -> bool {
    let total = cgroup::total_ram_bytes().unwrap_or(0);
    if total == 0 {
        return false; // can't reason about proportion — don't pin
    }
    let footprint = cgroup::read_cgroup_u64(cgroup_path, "memory.current").unwrap_or(u64::MAX);
    footprint <= total / 8
}

/// Pin a cgroup out of swap entirely (`Some(0)`) or hand it back (`None` → "max").
/// Best-effort: a kernel without swap accounting simply has no such file, and a
/// missing knob must never take the protector down.
fn set_swap_max(cgroup_path: &Path, bytes: Option<u64>) {
    let v = match bytes {
        Some(b) => b.to_string(),
        None => "max".to_string(),
    };
    let _ = cgroup::write_cgroup(cgroup_path, "memory.swap.max", &v);
}

fn page_size() -> u64 {
    let ps = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) };
    if ps > 0 { ps as u64 } else { 4096 }
}

/// Most we'll fault back in for one cgroup in one pass.
///
/// Deliberately small, because this work happens ON the daemon's 1s poll thread:
/// every page we recall off the disk swapfile is a synchronous major fault, so a
/// large budget converts a protection pass into a multi-second stall of the very
/// loop that is supposed to be watching for trouble. At ~100MB/s off the swapfile,
/// 64MB is ~0.6s worst case.
///
/// It doesn't need to be big. The protection pass repeats every 30s and the work
/// is self-limiting — a page we pull in has no swap PTE next pass and is never
/// touched again — so this is a *rate*, not a cap on total healing. The spine's
/// small units clear in one or two passes, and a compositor with a large anon
/// backlog drains at 64MB per pass in the background, which is exactly the right
/// urgency for a machine that is by definition healthy right now (see
/// FAULT_IN_MIN_AVAILABLE).
///
/// Note this is a budget PER CGROUP per pass, not per pass overall. The worst case
/// is therefore (spine classes x 64MB) of major faults on one poll tick. That has
/// never been observed — only the compositor has ever carried a backlog this size,
/// and everything else clears in one pass and stays clear — but if a cold session
/// ever does make every class hit its budget at once, this is the knob that stalls
/// the poll loop, and the fix is to thread one budget through the whole pass rather
/// than to shrink this number.
const FAULT_IN_BUDGET: u64 = 64 * 1024 * 1024;

/// Below this much MemAvailable, don't fault anything in. Faulting pages back
/// consumes the exact resource that is scarce during an incident, so this must
/// never fire while the machine is in trouble — it is a heal-while-healthy move,
/// and doing it under pressure would deepen the hole rather than fill it.
const FAULT_IN_MIN_AVAILABLE: u64 = 2 * 1024 * 1024 * 1024;

fn mem_available() -> u64 {
    let Ok(info) = std::fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    for line in info.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 =
                rest.trim().trim_end_matches("kB").trim().parse().unwrap_or(0);
            return kb * 1024;
        }
    }
    0
}

/// The address ranges of one pid that actually have pages out on swap, read from
/// `/proc/<pid>/smaps`.
///
/// Precision is the whole point. The obvious implementation — touch every
/// readable mapping — works but is wildly wasteful: measured on IBus it faulted
/// 108.8M to recover 18.3M of swap, because most mappings are file-backed shared
/// libraries whose pages don't live on swap at all. smaps reports `Swap:` per
/// mapping, so we can touch only the regions that have something to recall.
fn swapped_regions(pid: &str) -> Vec<(u64, u64)> {
    match std::fs::read_to_string(format!("/proc/{pid}/smaps")) {
        Ok(smaps) => parse_swapped_regions(&smaps),
        Err(_) => Vec::new(), // pid exited between listing and reading — normal
    }
}

/// Pure parser for smaps text — the testable core of `swapped_regions`.
fn parse_swapped_regions(smaps: &str) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut current: Option<(u64, u64)> = None;
    let mut readable = false;

    for line in smaps.lines() {
        // A mapping header looks like "7f8e40000000-7f8e40021000 rw-p 00000000 ...";
        // every other line is "Key: <n> kB". Only a header's first field parses as
        // a hex-hex range, which is what distinguishes them.
        if let Some((range, rest)) = line.split_once(' ') {
            if let Some((lo, hi)) = range.split_once('-') {
                if let (Ok(lo), Ok(hi)) =
                    (u64::from_str_radix(lo, 16), u64::from_str_radix(hi, 16))
                {
                    // PROT_NONE guard pages and the kernel's [vvar]/[vsyscall]
                    // pseudo-mappings can't be read; skip rather than collect EIO.
                    readable = rest.starts_with('r');
                    current = Some((lo, hi));
                    continue;
                }
            }
        }
        if let Some(rest) = line.strip_prefix("Swap:") {
            let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().unwrap_or(0);
            if kb > 0 && readable {
                if let Some(region) = current {
                    out.push(region);
                }
            }
        }
    }
    out
}

/// Bit 62 of a /proc/<pid>/pagemap entry: this page is swapped out.
/// (Bit 63 is "present". The PFN bits are zeroed without CAP_SYS_ADMIN, but these
/// two flags are readable regardless — and they are all we need.)
const PAGEMAP_SWAPPED: u64 = 1 << 62;

/// Of the pages in `regions`, exactly the ones currently out on swap.
///
/// smaps answers "does this MAPPING have swapped pages", which is far too coarse
/// to act on: a shmem VMA can be hundreds of MB with a handful of swapped pages
/// scattered inside it. Touching the whole region to find them spends the entire
/// budget faulting pages that were already resident and never reaches the ones
/// that matter — measured on gnome-shell as "recalled 0B" after touching a full
/// 64MB. pagemap answers the question per PAGE, so we touch only what's actually
/// out. This is the difference between a budget that measures effort and a budget
/// that measures progress.
fn swapped_pages(pid: &str, regions: &[(u64, u64)], limit: usize) -> Vec<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut pagemap) = std::fs::File::open(format!("/proc/{pid}/pagemap")) else {
        return Vec::new();
    };
    let ps = page_size();
    let mut out = Vec::new();
    let mut entry = [0u8; 8];

    for &(lo, hi) in regions {
        let mut addr = lo;
        while addr < hi {
            if out.len() >= limit {
                return out;
            }
            // One 8-byte entry per page, indexed by page number.
            let offset = (addr / ps) * 8;
            if pagemap.seek(SeekFrom::Start(offset)).is_ok()
                && pagemap.read_exact(&mut entry).is_ok()
                && u64::from_ne_bytes(entry) & PAGEMAP_SWAPPED != 0
            {
                out.push(addr);
            }
            addr += ps;
        }
    }
    out
}

/// Read one byte at each given address, faulting those pages back into RAM.
/// Returns bytes touched.
///
/// Reading another process's memory needs PTRACE_MODE_ATTACH, which Yama
/// (ptrace_scope=1) denies even to the same uid — so this needs CAP_SYS_PTRACE,
/// which the unit already carries for reading /proc/<pid>/cwd. It is only ever a
/// read: the target is never stopped, attached to, or modified.
fn touch_addresses(pid: &str, addrs: &[u64]) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut mem) = std::fs::File::open(format!("/proc/{pid}/mem")) else {
        return 0;
    };
    let ps = page_size();
    let mut touched = 0u64;
    let mut byte = [0u8; 1];

    for &addr in addrs {
        // A page that moved since we read pagemap simply errors — the process is
        // running and its map changes underneath us. Skip and continue.
        if mem.seek(SeekFrom::Start(addr)).is_ok() && mem.read(&mut byte).is_ok() {
            touched += ps;
        }
    }
    touched
}

/// Fault a protected cgroup's swapped-out pages back into RAM — the curative half
/// of pinning. Returns bytes touched (0 if there was nothing out, which is the
/// steady state and the common case).
///
/// Proven on this machine 2026-07-14, before/after:
///
///     IBus         swapped 18.3M -> 2.5M    (86% recalled)
///     XSettings    swapped  9.7M -> 0.9M    (91%)
///     wireplumber  swapped  7.8M -> 2.3M    (71%)
///     gnome-shell  faulted in 64M in one pass (hit the budget, drained over several)
///
/// Each of those units then sits at a small residue that never moves again. That
/// residue is the shmem tail — see the `else` arm at the bottom of this function.
/// The units whose latency the user actually feels (input method, audio, session
/// bus, the terminal they are typing in) are anon-heavy and heal to near-zero.
///
/// Only ever called for `swap_pin_eligible` cgroups that we've just set
/// memory.swap.max=0 on, so what we pull in cannot immediately be evicted again —
/// otherwise this would be a treadmill, fighting the kernel's reclaim in a loop.
fn fault_in_swapped(cgroup_path: &Path) -> u64 {
    // Every early return here is instrumented. An un-instrumented failure in this
    // function is indistinguishable from "there was nothing to do", because both
    // look like swap sitting still — which is exactly how the first version of
    // this shipped, and exactly how long it took to notice (one sample).
    let avail = mem_available();
    if avail < FAULT_IN_MIN_AVAILABLE {
        return 0; // healing is a healthy-state activity; don't dig during an incident
    }
    let before = cgroup::read_cgroup_u64(cgroup_path, "memory.swap.current").unwrap_or(0);
    if before == 0 {
        return 0; // nothing out — the steady state, and the common case
    }
    let Ok(procs) = std::fs::read_to_string(cgroup_path.join("cgroup.procs")) else {
        eprintln!("fault-in: {} — cannot read cgroup.procs", cgroup_path.display());
        return 0;
    };

    let mut total = 0u64;
    let mut pids = 0usize;
    let mut unreadable = 0usize;
    for pid in procs.lines() {
        let pid = pid.trim();
        if pid.is_empty() {
            continue;
        }
        pids += 1;
        let regions = swapped_regions(pid);
        if regions.is_empty() {
            continue;
        }
        // Budget in PAGES ACTUALLY OUT, not pages touched. The distinction is the
        // whole fix: the old code spent its budget reading resident pages inside
        // big shmem VMAs and recalled nothing.
        let room = (FAULT_IN_BUDGET.saturating_sub(total) / page_size()) as usize;
        if room == 0 {
            break;
        }
        let addrs = swapped_pages(pid, &regions, room);
        if addrs.is_empty() {
            continue; // smaps flagged the mapping, pagemap says nothing is out
        }
        let touched = touch_addresses(pid, &addrs);
        if touched == 0 {
            unreadable += 1; // pages were out but /proc/<pid>/mem wouldn't read
        }
        total += touched;
        if total >= FAULT_IN_BUDGET {
            break;
        }
    }

    let after = cgroup::read_cgroup_u64(cgroup_path, "memory.swap.current").unwrap_or(before);
    let leaf = cgroup_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if total > 0 {
        // Report what WE did (`total` — bytes touched at addresses pagemap said were
        // out, each one a forced major fault) separately from what the cgroup counter
        // did. They are not the same claim: the target is a live process the kernel is
        // concurrently faulting in and evicting, so `before - after` folds its activity
        // into ours. Printing that delta as "recalled" would have us taking credit for
        // the kernel's work — and, when a cgroup's swap moves on its own while we touch
        // nothing, inventing a success outright.
        eprintln!(
            "fault-in: {leaf} faulted in {} (cgroup swap {} -> {})",
            format_bytes(total),
            format_bytes(before),
            format_bytes(after)
        );
    } else if unreadable > 0 {
        // Pages were demonstrably out and we could not read them. This one IS a
        // defect — most likely a missing CAP_SYS_PTRACE.
        eprintln!(
            "fault-in: {leaf} has {} swapped but /proc/<pid>/mem was unreadable for \
             {unreadable}/{pids} pid(s) — check CAP_SYS_PTRACE",
            format_bytes(before)
        );
    } else {
        // Expected, not broken. Every pid's page table is clean: pagemap found no
        // swap PTE anywhere, so there is no address whose touch would pull anything
        // back. That is the signature of shmem swap (tmpfs / wl_shm / dma-buf) —
        // the shmem inode owns the swap slot, not the mapper, so memory.swap.current
        // counts it while no process admits to it. Nothing to do; the swap.max=0 set
        // just above is the whole defence.
        //
        // Measured on gnome-shell 2026-07-14, the honest version of this claim:
        //
        //     smaps Swap, summed over all 6 pids :  13MB   <-- all that is touchable
        //     memory.swap.current                : 474MB
        //     memory.stat shmem (resident)       : 461MB   <-- same order as the gap
        //
        // NOTE for whoever revisits this: do NOT try to prove shmem-vs-anon by
        // comparing `memory.stat anon` to `memory.swap.current`. An earlier version
        // of this comment argued "anon=139MB < swap=476MB, so most of the swap can't
        // be anon" — which is wrong, and wrong in a way that looks convincing. `anon`
        // counts RESIDENT anonymous memory; a page that swaps out leaves that counter
        // by definition. The comparison is vacuous. smaps vs swap.current is the real
        // discriminator, because it asks the only question that matters here: is there
        // a swap PTE to touch?
        eprintln!(
            "fault-in: {leaf} {} swapped, none reachable — no swap PTE in any pid \
             (shmem/tmpfs); pinned going forward",
            format_bytes(before)
        );
    }
    total
}

/// Register (or update) a protection, then reconcile the tree. Returns an error
/// only if the *target* itself couldn't be protected (e.g. no write permission),
/// so callers like startup protection can still detect real failure.
fn set_protection(cgroup_path: &Path, value: u64) -> Result<()> {
    // Align down to a page multiple. The kernel floors memory.min to a page, so
    // an unaligned target (e.g. total_ram/100) is stored a few bytes short and
    // `got >= value` below would never hold — which made the daemon report every
    // protection as failed, retry forever, and never announce success even though
    // the protection was in fact applied. Losing <1 page of protection is
    // irrelevant; the false-failure it prevents is not.
    let value = value / page_size() * page_size();
    {
        let mut prot = PROTECTIONS.lock().unwrap_or_else(|e| e.into_inner());
        prot.insert(cgroup_path.to_path_buf(), value);
    }
    apply();
    // Also tell systemd-oomd to leave this cgroup alone (memory.min doesn't stop
    // oomd — only this xattr does). Set on the protected leaf, not its ancestors,
    // so oomd still has other cgroups to pick from if it must act.
    if value > 0 {
        set_oomd_avoid(cgroup_path, true);
        // Forbid swapping outright. memory.min only stops reclaim from taking a
        // cgroup BELOW its floor — everything above the floor is still fair game,
        // so a compositor guaranteed 516MB with a ~1GB working set was measured
        // half-evicted: 523MB resident against 530MB in swap, with
        // memory.swap.max sitting wide open at "max". Every window switch then
        // faults those pages back, and since zram had long since filled (6.5/7.4GB)
        // they were coming off the on-disk swapfile — which is exactly what "I can
        // barely switch windows and the keyboard lags" is. vm.swappiness is 180
        // here (correct when zram is fast and free, ruinous once it's full), so the
        // kernel pages the desktop out enthusiastically unless told not to.
        //
        // Protecting the spine from the OOM killer while letting it swap to disk
        // protects the wrong thing: it survives, unusably. This is the lever that
        // makes "resident" actually mean resident.
        //
        // Only for the SMALL things, though. set_protection also runs for the
        // focused app (protect_foreground), and pinning a 4GB browser wholly into
        // RAM would reserve a quarter of the machine and manufacture the very
        // pressure we exist to prevent — favouring the foreground must not turn it
        // into a black hole. The spine and an ordinary terminal are ~0.2-1GB and
        // fit comfortably; anything larger keeps its swap door open and is welcome
        // to page out its cold parts.
        if swap_pin_eligible(cgroup_path) {
            set_swap_max(cgroup_path, Some(0));
            // ...and bring home what's ALREADY gone. memory.swap.max=0 only forbids
            // *future* eviction; it does not recall a page the kernel wrote out an
            // hour ago, so a service swapped out before rtux protected it stays on
            // disk and the user keeps paying a major fault per keystroke. Measured
            // 2026-07-14: IBus sat at 5.3M resident against 18.3M swapped — 78% of
            // the input method on a disk swapfile — with the pin already applied.
            //
            // Pinning without recalling is therefore only half a protection: it
            // guarantees the spine won't get *worse*, and leaves it broken.
            fault_in_swapped(cgroup_path);
        } else {
            set_swap_max(cgroup_path, None);
        }
    } else {
        // Releasing a protection re-opens the swap door.
        set_swap_max(cgroup_path, None);
    }
    let got = cgroup::read_cgroup_u64(cgroup_path, "memory.min").unwrap_or(0);
    if value == 0 || got >= value {
        Ok(())
    } else {
        anyhow::bail!(
            "memory.min on {} read back as {} after requesting {} (page {})",
            cgroup_path.display(),
            got,
            value,
            page_size()
        )
    }
}

/// Drop a protection, then reconcile — lowering the ancestor residue it leaves
/// behind to whatever the *remaining* protections still need (or zero).
fn clear_protection(cgroup_path: &Path) {
    {
        let mut prot = PROTECTIONS.lock().unwrap_or_else(|e| e.into_inner());
        prot.remove(cgroup_path);
    }
    set_oomd_avoid(cgroup_path, false);
    apply();
}

/// One class of the interactive spine: the processes a keystroke, a sound, or a
/// frame physically passes through on its way to the user.
struct SpineClass {
    /// Label for logs and notifications.
    name: &'static str,
    /// Substrings matched against unit cgroup names. EVERY match is protected,
    /// so a substring may deliberately span several units.
    units: &'static [&'static str],
    /// The memory.min budget each matching cgroup gets, as a fn of total RAM.
    budget: fn(u64) -> u64,
}

/// The interactive spine, in full.
///
/// The membership rule is mechanical, not editorial: a unit belongs here iff the
/// user's input, audio, or drawing *traverses it synchronously*. If it can be
/// paged out without the desktop feeling broken, it is not spine — it is an app,
/// and apps are expendable (that's the whole architecture: spine pinned hard,
/// apps ruthlessly throttled).
///
/// Why the list is this long: rtux originally protected only compositor + audio
/// + bus, and the other eight units were left at `memory.min=0, oom_score_adj=+200`
/// — i.e. unprotected AND volunteering to be the OOM victim. Measured on this
/// machine 2026-07-14, mid-incident:
///
///   IBus (every keystroke)   res=  5.3M  swap= 18.4M   <-- 78% on disk
///   wireplumber (audio route) res= 7.6M  swap=  7.9M
///   XSettings (GTK theme/DPI) res= 2.7M  swap=  9.7M   <-- 78% on disk
///   xdg-desktop-portal-gnome  res= 4.3M  swap=  7.6M
///
/// That is the ~19s keyboard latency, in a table. The input method was on a disk
/// swapfile, so every keypress waited on a major fault. The entire cost of never
/// letting that happen again is ~160MB on a 14.8GB machine — under 1.1% of RAM.
/// Protecting a compositor while the input method it draws for is on disk is not
/// a partial win; it draws a responsive-looking desktop that cannot be typed into.
///
/// NOTE: pinning is prophylactic, not curative. memory.swap.max=0 forbids *future*
/// eviction; it does not fault already-swapped pages back in. Units already on
/// disk when protection lands stay there until next touch (one fault, once).
const SPINE: &[SpineClass] = &[
    // The compositor: every frame. Biggest budget — it holds the scene graph.
    SpineClass {
        name: "compositor",
        units: &["gnome-shell", "gnome.Shell", "kwin", "sway"],
        budget: compositor_memory_min,
    },
    // Input methods: EVERY keystroke traverses these. The single hottest path in
    // the set and, until now, entirely unprotected.
    SpineClass {
        name: "input method",
        units: &["IBus", "ibus", "fcitx"],
        budget: spine_memory_min,
    },
    // Audio. "pipewire" spans pipewire.service and pipewire-pulse.service;
    // wireplumber is pipewire's session/policy manager — without it resident,
    // pipewire routes nothing, so protecting pipewire alone was half a fix.
    SpineClass {
        name: "audio",
        units: &["pipewire", "pulseaudio", "wireplumber"],
        budget: audio_memory_min,
    },
    // The session message bus. When the kernel global OOM killer took the machine
    // down on 2026-07-14 it picked *dbus.service* as its victim — killing the bus
    // collapses the whole graphical session (gnome-session, portals, gdm all fall
    // with it). memory.min alone can't prevent that (the global killer ignores
    // it), so the bus joins the spine and gets oom_score_adj like the rest.
    // Match the exact ".service" so we don't grab at-spi-dbus-bus.service instead.
    SpineClass { name: "session bus", units: &["dbus.service"], budget: spine_memory_min },
    // Settings daemons: keyboard layout and shortcuts, media/volume/brightness
    // keys, and XSettings — which GTK apps block on for theme, font, and DPI.
    // The substring spans the whole org.gnome.SettingsDaemon.* family on purpose;
    // they are single-digit MB each and collectively cheaper than one browser tab.
    SpineClass {
        name: "settings daemon",
        units: &["SettingsDaemon", "dconf.service"],
        budget: spine_memory_min,
    },
    // Portals: file dialogs, screenshots, screen sharing. Not per-frame, but a
    // swapped-out portal presents as an app hanging on open-file, which is
    // indistinguishable from the desktop being broken.
    SpineClass {
        name: "portal",
        units: &["xdg-desktop-portal", "xdg-document-portal"],
        budget: spine_memory_min,
    },
];

/// oom_score_adj we bias the session spine to: -1000, the maximum protection.
///
/// It has to be the max, not merely "very negative". Measured on this machine
/// (2026-07-14): dbus.service sat at +200 (practically volunteering to be the
/// OOM victim — which is exactly what happened), while the actual memory hogs —
/// Claude Code sessions — self-protect at oom_score_adj=-1000. Anything weaker
/// than -1000 on the spine would still be picked *before* a -1000 hog, so the
/// spine must at least tie them. Once spine and hogs are both at -1000 the kernel
/// falls back to raw memory size and kills the *largest* -1000 process — a hog,
/// not tiny dbus. The daemon itself is also -1000 but small, so it survives that
/// size tiebreak too.
///
/// IMPORTANT: only apply this to spine services that are NOT process-ancestors
/// of the memory hogs. oom_score_adj is inherited at fork, so setting it on
/// `systemd --user` (init.scope) would leak -1000 to every user service it
/// spawns — including the terminal/Claude/build hogs — making them *even more*
/// unkillable. The bus and compositor don't fork the hogs (terminals do), so
/// they're safe targets. (The hogs self-protecting at -1000 is its own problem,
/// addressed by the explicit kill rung — a SIGKILL ignores oom_score_adj.)
const OOM_SCORE_ADJ_PROTECT: i32 = -1000;

/// oom_score_adj pushed onto BACKGROUND HOGS so the kernel's *global* OOM killer
/// picks one of them rather than the session.
///
/// Protecting the spine at -1000 is only half the job, and on 2026-07-14 the
/// missing half cost a whole session. The kernel's global killer ranks by
/// oom_score_adj, and the heaviest consumers here self-protect: every Claude
/// session sits at -1000 by its own hand. So when RAM+swap ran out, ~7GB of hogs
/// were *structurally immune* and the only eligible victims left were the session
/// itself — session dbus and pipewire at +200, and `systemd --user` at +100.
/// The kernel dutifully killed the user manager, which IS the logout.
///
/// Being merely un-killable is not enough: someone must be *more* killable. This
/// inverts the ranking so a resumable background session takes the hit instead of
/// the desktop. Raising a score needs no privilege the daemon lacks, and it
/// deliberately overrides a hog's own -1000 — self-protection is antisocial when
/// the alternative victim is the user's whole session. Reset to neutral on
/// recovery; re-applied every pass while pressure lasts (new forks inherit it).
const OOM_SCORE_ADJ_HOG: i32 = 500;
/// The neutral score a hog returns to once pressure clears. Deliberately NOT the
/// -1000 it set for itself: we do not restore antisocial self-protection, we just
/// stop actively biasing the kernel toward it.
const OOM_SCORE_ADJ_NEUTRAL: i32 = 0;

/// Bias the kernel's global OOM killer TOWARD this cgroup — the mirror of
/// `protect_one`'s -1000. Best-effort per pid; a process that exits mid-walk is
/// simply skipped.
pub fn bias_hog_oom(cgroup_path: &Path) {
    set_oom_score_adj(cgroup_path, OOM_SCORE_ADJ_HOG);
}

/// Undo `bias_hog_oom` — back to neutral, not to the hog's own -1000.
pub fn unbias_hog_oom(cgroup_path: &Path) {
    set_oom_score_adj(cgroup_path, OOM_SCORE_ADJ_NEUTRAL);
}

/// cpu.weight for the desktop slice (session.slice) — ~10× the app-slice default
/// so the compositor wins the scheduler over bulk app work under contention.
/// Work-conserving, so it costs nothing when the desktop is idle.
const CPU_WEIGHT_DESKTOP: u32 = 1000;
/// cpu.weight for the focused app — favours it among its app-slice siblings (the
/// scheduler dual of the foreground memory pin). Reset to default on focus change.
const CPU_WEIGHT_FOREGROUND: u32 = 1000;
/// The kernel default cpu.weight — what a released foreground leaf returns to.
const CPU_WEIGHT_DEFAULT: u32 = 100;

/// Give the desktop slice priority over bulk app work under CPU contention.
/// `cpu.weight` is proportional *among siblings*, so the lever that favours the
/// compositor (in session.slice) over the hogs (in app.slice) is session.slice's
/// weight at the user@.service level — not the compositor leaf itself. Enabling
/// the cpu controller down to session.slice makes its cpu.weight writable.
fn set_desktop_cpu_priority(compositor_path: &Path) {
    let Some(session_slice) = compositor_path.parent() else { return };
    cgroup::ensure_cpu_controller(compositor_path);
    if let Err(e) = crate::actions::set_cpu_weight(session_slice, CPU_WEIGHT_DESKTOP) {
        eprintln!("  note: could not set desktop cpu.weight ({e})");
    }
    // Also enable the cpu controller on the sibling app.slice now, so foreground
    // leaf boosts and background throttling can set cpu.weight there without
    // waiting for the first focus event. (app.slice is not on the compositor's
    // ancestor chain, so the call above doesn't reach it.)
    if let Some(user_service) = session_slice.parent() {
        cgroup::ensure_cpu_controller(&user_service.join("app.slice").join("_"));
    }
}

pub struct ProtectedService {
    /// Human-facing label, e.g. "compositor (org.gnome.Shell@ubuntu.service)".
    /// For display only — never match on it (see `class`).
    pub name: String,
    /// The SPINE class this came from, e.g. "compositor". Stable, and the ONLY
    /// thing code should dispatch on.
    ///
    /// This exists because `name` gained its " (leaf)" suffix in da8acac, which
    /// silently broke the one place that matched it — `find(|s| s.name ==
    /// "compositor")` — and with it the standing CPU boost, with no error and no
    /// log line, because a `find` that matches nothing looks exactly like a
    /// compositor that wasn't found. Display strings are for humans; they change
    /// whenever the log wording changes, so they must not be load-bearing.
    pub class: &'static str,
    pub cgroup_path: PathBuf,
    pub memory_min: u64,
}

/// Outcome of one protection pass. Each critical service is attempted
/// independently: `protected` holds those secured this pass, `failed` holds the
/// ones we tried but couldn't (with why), so the caller can report them instead
/// of the failure vanishing.
pub struct ProtectionReport {
    pub protected: Vec<ProtectedService>,
    pub failed: Vec<(&'static str, String)>,
}

/// Calculate memory.min for the compositor based on hardware heuristics.
/// Uses a percentage of total RAM with floor/ceiling:
///   - 3% of total RAM
///   - minimum 256MB
///   - maximum 1GB
fn compositor_memory_min(total_ram: u64) -> u64 {
    let three_pct = total_ram / 33; // ~3%
    let min = 256 * 1024 * 1024; // 256MB
    let max = 1024 * 1024 * 1024; // 1GB
    three_pct.clamp(min, max)
}

/// Calculate memory.min for audio services.
/// Smaller budget: 1% of RAM, floor 64MB, ceiling 256MB.
fn audio_memory_min(total_ram: u64) -> u64 {
    let one_pct = total_ram / 100;
    let min = 64 * 1024 * 1024; // 64MB
    let max = 256 * 1024 * 1024; // 256MB
    one_pct.clamp(min, max)
}

/// The standing ceiling on what ALL apps may collectively hold resident:
/// everything except a reserve kept for the spine, the kernel, and page cache.
///
/// Reserve is a quarter of RAM, clamped to [2GB, 4GB] — enough for the spine
/// (~1.3GB here) plus room for the kernel and enough page cache that the desktop
/// isn't re-reading its own binaries off disk, without handing a large machine a
/// pointlessly large idle reserve.
fn app_slice_high(total_ram: u64) -> u64 {
    let reserve = (total_ram / 4).clamp(2 * 1024 * 1024 * 1024, 4 * 1024 * 1024 * 1024);
    total_ram.saturating_sub(reserve)
}

/// Put a standing ceiling on bulk app memory. Written ONCE at startup — not in
/// response to pressure — because that is the entire point.
///
/// Measured on this machine 2026-07-14, at rest, before this existed:
///
///     app.slice      current=8809M  swap=4140M   high=max  max=max
///     session.slice  current= 741M  swap= 561M
///     app.slice memory.peak (this boot) = 11839M   on 14.8G of RAM
///
/// `app.slice` had no ceiling of any kind, so apps were free to grow until the
/// kernel evicted whatever it could — which is why 561MB of the *spine* was on
/// disk while the machine was idle. rtux's answer to that used to be the reactive
/// ladder, but by the time PSI reports a stall the eviction has already happened;
/// PSI is a lagging indicator, and you cannot react your way to "never janked".
/// This is the leading one: a ceiling that is simply always true.
///
/// `memory.high` (not `memory.max`) is deliberate. high throttles the cgroup into
/// direct reclaim — apps get slower and their cold pages go to zram — and it never
/// invokes the OOM killer. max would start killing. The trade this encodes is the
/// stated architecture: the spine is pinned hard, apps are expendable, and an app
/// going slow is always preferable to the desktop going away.
///
/// Fully reversible: write "max" back, or stop the daemon and the cgroup is
/// recreated unbounded on next login.
fn set_bulk_ceiling(compositor_path: &Path) {
    let Some(user_service) = compositor_path.parent().and_then(|p| p.parent()) else { return };
    let app_slice = user_service.join("app.slice");
    if !app_slice.is_dir() {
        return;
    }
    let Ok(total_ram) = cgroup::total_ram_bytes() else { return };
    let high = app_slice_high(total_ram);
    match crate::actions::cap_cgroup(&app_slice, high) {
        Ok(()) => eprintln!(
            "bulk ceiling: apps capped at {} resident ({} reserved for desktop + kernel)",
            format_bytes(high),
            format_bytes(total_ram.saturating_sub(high))
        ),
        Err(e) => eprintln!("  note: could not set app.slice memory.high ({e})"),
    }
}

/// Discover and protect compositor + audio services.
/// Returns the list of services that were protected.
pub fn protect_critical_services() -> Result<ProtectionReport> {
    let total_ram = cgroup::total_ram_bytes()?;
    let mut report = ProtectionReport { protected: Vec::new(), failed: Vec::new() };

    // Attempt each service INDEPENDENTLY. A failure in one (its cgroup not up
    // yet, its lookup erroring, or an unwritable memory.min) must never abort
    // protection of the others. This routine previously used `?` on each branch,
    // so an error protecting *audio* discarded an already-successful *compositor*
    // protection — the compositor's memory.min was written as a side effect, but
    // the function returned Err, the daemon reported perpetual failure, and it
    // retried (silently) every 30s forever while the compositor was in fact
    // protected. The compositor is the load-bearing one for responsiveness;
    // never let audio's fate mask it.
    for class in SPINE {
        protect_one(class.name, class.units, (class.budget)(total_ram), &mut report);
    }

    // Standing config, applied once the compositor gives us the session layout.
    // Both of these are set-and-forget: they are true before pressure arrives,
    // which is what the reactive ladder can never be.
    if let Some(comp) = report.protected.iter().find(|s| s.class == "compositor") {
        // A ceiling on collective app memory, so bulk work reclaims into zram
        // instead of evicting the spine. This is the load-bearing one.
        set_bulk_ceiling(&comp.cgroup_path);
        // Scheduler preference for the desktop. Weak by construction (cgroup v2
        // has no cpu.min — see the note in main.rs), but free and directionally
        // right. Not a guarantee, and not to be mistaken for one.
        set_desktop_cpu_priority(&comp.cgroup_path);
    }

    Ok(report)
}

/// Best-effort protection of one service class. On success it lands in
/// `report.protected`; on any failure it lands in `report.failed` with a reason
/// — nothing is logged here (this runs on a 30s retry, so the *caller* decides
/// when to log, to avoid spamming the journal every cycle).
fn protect_one(name: &'static str, services: &[&str], mem_min: u64, report: &mut ProtectionReport) {
    // EVERY matching cgroup, not just the first. A category can span several units
    // and the first match is not a representative of the rest: "pipewire" matches
    // pipewire-pulse.service AND pipewire.service, and protecting only the former
    // left the real audio daemon at +200 while the log claimed audio was protected.
    let paths = match cgroup::find_all_cgroups_for_service(services) {
        Ok(p) => p,
        Err(e) => {
            report.failed.push((name, format!("lookup failed: {e}")));
            return;
        }
    };
    if paths.is_empty() {
        report.failed.push((name, "cgroup not present yet".to_string()));
        return;
    }
    for path in paths {
        match set_protection(&path, mem_min) {
            Ok(()) => {
                // memory.min + oomd_avoid fend off reclaim and systemd-oomd, but
                // NOT the kernel's global OOM killer — only oom_score_adj sways
                // that. Bias it away from the spine so a global OOM (RAM+swap
                // both full) kills a hog, not the desktop.
                set_oom_score_adj(&path, OOM_SCORE_ADJ_PROTECT);
                // Name each unit by its own leaf, so the caller's announce-once
                // ledger reports "audio (pipewire.service)" separately from
                // "audio (pipewire-pulse.service)" instead of collapsing them and
                // hiding a gap behind an already-announced name.
                let leaf = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                report.protected.push(ProtectedService {
                    name: format!("{name} ({leaf})"),
                    class: name,
                    cgroup_path: path,
                    memory_min: mem_min,
                });
            }
            Err(e) => report.failed.push((name, e.to_string())),
        }
    }
}

/// Session-spine services (the message bus) are small; a modest floor keeps their
/// working set resident under reclaim. Their real protection is oom_score_adj —
/// this just stops routine paging from touching them. ~0.5% of RAM, 32–128MB.
fn spine_memory_min(total_ram: u64) -> u64 {
    (total_ram / 200).clamp(32 * 1024 * 1024, 128 * 1024 * 1024)
}

/// Bias the kernel's *global* OOM killer away from a cgroup's processes by
/// writing a strongly-negative oom_score_adj to each live pid. memory.min and
/// the oomd_avoid xattr don't influence the global killer's victim choice; this
/// does. Best-effort per pid (a pid can exit mid-loop); children inherit the adj
/// at fork, and the daemon's retry loop re-applies it for any respawns. Only
/// called on spine services that don't fork the hogs (see OOM_SCORE_ADJ_PROTECT).
fn set_oom_score_adj(cgroup_path: &Path, adj: i32) {
    let Ok(procs) = std::fs::read_to_string(cgroup_path.join("cgroup.procs")) else {
        return;
    };
    for pid in procs.lines() {
        let pid = pid.trim();
        if !pid.is_empty() {
            let _ = std::fs::write(format!("/proc/{pid}/oom_score_adj"), adj.to_string());
        }
    }
}

/// Pin a cgroup's current usage into RAM (protect it from reclaim/swap) by
/// setting memory.min to its live memory.current, propagating up ancestors.
/// Used by the HUD "Protect" action. Clamped to a fraction of RAM — same as
/// protect_foreground — so a client can't pin an arbitrarily large cgroup as
/// unreclaimable and starve the machine into OOM (the opposite of the goal).
pub fn protect_cgroup(cgroup_path: &std::path::Path) -> Result<()> {
    let current = cgroup::read_cgroup_u64(cgroup_path, "memory.current").unwrap_or(0);
    let total = cgroup::total_ram_bytes().unwrap_or(0);
    let cap = if total > 0 { total / 4 } else { 2 * 1024 * 1024 * 1024 };
    set_protection(cgroup_path, current.min(cap))
}

/// Release a protection *fully* — the leaf and any ancestor residue no longer
/// needed by other protections come back down. (Previously only the leaf was
/// cleared, so parent slices accumulated memory.min until reboot.)
pub fn unprotect_cgroup(cgroup_path: &std::path::Path) -> Result<()> {
    clear_protection(cgroup_path);
    // Release the OOM bias too. Leaving it would make every app that was EVER
    // focused permanently unkillable, and a global OOM must always have somewhere
    // to go — an accumulating set of -1000 leaves is how the kernel ends up with
    // only the session left to kill (2026-07-14). Neutral, not the hog bias: this
    // app was legitimately in use a moment ago.
    set_oom_score_adj(cgroup_path, OOM_SCORE_ADJ_NEUTRAL);
    // Drop any foreground CPU boost back to default when focus leaves this app
    // (harmless no-op for a leaf that was never boosted).
    let _ = crate::actions::set_cpu_weight(cgroup_path, CPU_WEIGHT_DEFAULT);
    Ok(())
}

/// Protect the *focused* app: keep its pages resident so the window you're in is
/// never paged out and always instant. Clamped to a fraction of RAM so a huge
/// foreground app (a big browser) can't reserve the whole machine and starve
/// everything else — foreground is favoured, not made a black hole.
pub fn protect_foreground(cgroup_path: &std::path::Path) -> Result<()> {
    let current = cgroup::read_cgroup_u64(cgroup_path, "memory.current").unwrap_or(0);
    let total = cgroup::total_ram_bytes().unwrap_or(0);
    let cap = if total > 0 { total / 4 } else { 2 * 1024 * 1024 * 1024 };
    let result = set_protection(cgroup_path, current.min(cap));
    // Pinning WITHOUT biasing the OOM killer is not protection — it is an
    // accelerant. memory.min + memory.swap.max=0 keep pages RESIDENT, and the
    // kernel's oom_badness() scores candidates on RSS. So every byte we pin
    // raises this cgroup's OOM score: the harder we "protect" the focused app,
    // the more attractive a victim it becomes. memory.min does not prevent death,
    // it converts reclaim pressure into kill pressure — and only oom_score_adj
    // speaks to the global killer. protect_one has always paired the two for the
    // spine; the foreground path never did, so rtux was actively marking the
    // window you work in for death while reporting it as protected.
    set_oom_score_adj(cgroup_path, OOM_SCORE_ADJ_PROTECT);
    // CPU: favour the focused app among its slice siblings — the scheduler dual
    // of the memory pin. unprotect_cgroup drops it back to default on focus change.
    cgroup::ensure_cpu_controller(cgroup_path);
    let _ = crate::actions::set_cpu_weight(cgroup_path, CPU_WEIGHT_FOREGROUND);
    result
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.0}MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0}KB", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::desired_min_map;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    const BASE: &str = "/sys/fs/cgroup";

    #[test]
    fn ancestors_take_max_and_release_cleanly() {
        let base = Path::new(BASE);
        let a = PathBuf::from("/sys/fs/cgroup/user.slice/app.slice/a.scope");
        let b = PathBuf::from("/sys/fs/cgroup/user.slice/app.slice/b.scope");
        let app = Path::new("/sys/fs/cgroup/user.slice/app.slice");
        let user = Path::new("/sys/fs/cgroup/user.slice");

        let mut p = HashMap::new();
        p.insert(a.clone(), 100u64);
        p.insert(b.clone(), 300u64);
        let d = desired_min_map(&p, base);
        assert_eq!(d[&a], 100);
        assert_eq!(d[&b], 300);
        assert_eq!(d[app], 300, "shared ancestor takes the max child");
        assert_eq!(d[user], 300);
        assert!(!d.contains_key(base), "cgroup root is never protected");

        // Releasing the bigger protection lowers the shared ancestors.
        p.remove(&b);
        let d = desired_min_map(&p, base);
        assert_eq!(d[app], 100, "ancestor drops to the remaining protection");
        assert_eq!(d[user], 100);

        // Releasing everything clears the map entirely — no residue.
        p.clear();
        assert!(desired_min_map(&p, base).is_empty());
    }
}

#[cfg(test)]
mod bulk_ceiling_tests {
    use super::app_slice_high;
    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn reserve_is_clamped_at_both_ends() {
        // Small box: a flat 25% would reserve only 1GB — less than the spine
        // itself needs — so the floor lifts it to 2GB.
        assert_eq!(app_slice_high(4 * GB), 2 * GB);
        // Large box: a flat 25% would idle 16GB for a spine that needs ~1.3GB,
        // so the ceiling caps the reserve at 4GB.
        assert_eq!(app_slice_high(64 * GB), 60 * GB);
    }

    #[test]
    fn this_machine_leaves_room_for_the_spine() {
        let total = 15204 * 1024 * 1024; // 14.8G, measured
        let high = app_slice_high(total);
        let reserve = total - high;
        // app.slice peaked at 11839M this boot with no ceiling, which is what
        // pushed 561M of session.slice onto disk. The ceiling must bite there.
        assert!(high < 11839 * 1024 * 1024, "ceiling must bite at the observed peak");
        // ...but must still leave the spine (~1.3G) real headroom.
        assert!(reserve > 3 * GB, "reserve too small for spine + cache");
    }

    #[test]
    fn never_underflows_on_a_tiny_machine() {
        // saturating_sub: a box smaller than the reserve floor yields 0, not a
        // wrapped-around 16-exabyte ceiling (which would be a silent no-op).
        assert_eq!(app_slice_high(1 * GB), 0);
    }
}

#[cfg(test)]
mod smaps_tests {
    use super::{parse_swapped_regions, swapped_regions};

    /// Real /proc/<pid>/smaps output (format captured verbatim from this kernel),
    /// doctored so the mappings differ in exactly what the parser must discriminate:
    /// swapped vs not, readable vs not. Includes the SwapPss: line the real file has.
    const SAMPLE: &str = "\
5af05e753000-5af05e7c4000 r--p 00000000 fc:01 3932305                    /usr/bin/rg
Size:                132 kB
KernelPageSize:        4 kB
Rss:                 132 kB
Private_Dirty:         0 kB
Anonymous:             0 kB
Swap:                  0 kB
SwapPss:               0 kB
VmFlags: rd mr mw me dw sd
5af060000000-5af060021000 rw-p 00000000 00:00 0                          [heap]
Size:                132 kB
KernelPageSize:        4 kB
Rss:                   8 kB
Private_Dirty:         8 kB
Anonymous:           132 kB
Swap:                124 kB
SwapPss:             124 kB
VmFlags: rd wr mr mw me ac sd
7f0000000000-7f0000002000 ---p 00000000 00:00 0
Size:                  8 kB
Rss:                   0 kB
Swap:                  8 kB
SwapPss:               8 kB
VmFlags: mr mw me sd
7ffd00000000-7ffd00003000 rw-p 00000000 00:00 0                          [stack]
Size:                 12 kB
Rss:                  12 kB
Anonymous:            12 kB
Swap:                 64 kB
SwapPss:              64 kB
VmFlags: rd wr mr mw me gd ac
";

    #[test]
    fn collects_only_readable_mappings_that_have_swap() {
        // [heap] (124 kB, rw-p) and [stack] (64 kB, rw-p) qualify.
        // /usr/bin/rg has Swap: 0 -> nothing to recall, skip.
        // The ---p guard page HAS swap but is unreadable -> touching it yields
        // only EIO, so skip it rather than burn syscalls failing.
        assert_eq!(
            parse_swapped_regions(SAMPLE),
            vec![(0x5af060000000, 0x5af060021000), (0x7ffd00000000, 0x7ffd00003000)]
        );
    }

    #[test]
    fn swappss_does_not_double_count() {
        // "SwapPss:" must not satisfy the "Swap:" prefix test. If it did, every
        // swapped mapping would be pushed twice — silently halving the effective
        // budget and doubling the syscalls. The 5th byte is 'P', not ':'.
        let regions = parse_swapped_regions(SAMPLE);
        let heap = (0x5af060000000u64, 0x5af060021000u64);
        assert_eq!(regions.iter().filter(|&&r| r == heap).count(), 1);
    }

    #[test]
    fn garbage_and_absent_pids_yield_nothing() {
        assert!(parse_swapped_regions("").is_empty());
        // A "Swap:" line with no preceding header must not invent a region.
        assert!(parse_swapped_regions("not smaps at all\nSwap: 12 kB\n").is_empty());
        // A pid that vanished mid-walk is normal, not an error.
        assert!(swapped_regions("nonexistent-pid").is_empty());
    }
}

#[cfg(test)]
mod spine_class_tests {
    use super::SPINE;

    #[test]
    fn compositor_class_exists_and_is_matchable() {
        // The standing config (bulk ceiling + desktop CPU boost) is gated on
        // finding class == "compositor". If that literal ever drifts from the
        // SPINE table, both silently stop running — which is exactly what
        // happened when the gate matched the display `name` instead, and the
        // name gained a " (leaf)" suffix. Nothing errored; they just stopped.
        assert!(
            SPINE.iter().any(|c| c.name == "compositor"),
            "SPINE has no class named \"compositor\" — the standing config gate in \
             protect_critical_services() will silently never fire"
        );
    }

    #[test]
    fn every_class_has_units_and_a_distinct_name() {
        let mut seen = std::collections::HashSet::new();
        for class in SPINE {
            assert!(!class.units.is_empty(), "spine class {} matches nothing", class.name);
            assert!(seen.insert(class.name), "duplicate spine class name {}", class.name);
        }
    }

    #[test]
    fn input_method_is_in_the_spine() {
        // The unit whose absence caused the ~19s keyboard latency: IBus sat at
        // memory.min=0 with 78% of itself on a disk swapfile. Regression guard.
        let input = SPINE.iter().find(|c| c.name == "input method").expect("no input method class");
        assert!(input.units.iter().any(|u| u.contains("IBus") || u.contains("ibus")));
    }
}
