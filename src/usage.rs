//! The Usage group of a running machine's details: what it has used of the host's
//! processors, memory, disks and network over the last minute, drawn as it comes.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::dialogs::size;
use crate::hypervisor::Usage;
use crate::machine_view::MachineView;
use crate::window::MachinesWindow;
use crate::{adw, glib, gtk};

const SAMPLES: usize = 60;
const INTERVAL: Duration = Duration::from_secs(1);
/// The least the disk and network graphs reach up to, in bytes a second, so that a trickle
/// does not fill them.
const DISK_FLOOR: f64 = 1024.0 * 1024.0;
const NET_FLOOR: f64 = 64.0 * 1024.0;

/// What a machine has used, sample by sample. The view keeps it, so that it outlives the
/// details page, which is built again at every change.
#[derive(Debug, Default)]
pub struct History {
    uuid: String,
    last: Option<(Instant, Usage)>,
    /// Whether a sample is on its way, so that two pages do not both ask.
    polling: bool,
    /// Percent of the machine's processors.
    cpu: VecDeque<f64>,
    /// Bytes.
    memory: VecDeque<f64>,
    /// Bytes a second, read and written together, and received and sent together.
    disk: VecDeque<f64>,
    net: VecDeque<f64>,
    rates: Rates,
}

#[derive(Debug, Default, Clone, Copy)]
struct Rates {
    cpu: f64,
    disk_read: f64,
    disk_written: f64,
    received: f64,
    sent: f64,
}

impl History {
    fn add(&mut self, now: Instant, usage: Usage) {
        if let Some((then, last)) = self.last {
            let seconds = now.duration_since(then).as_secs_f64();
            if seconds <= 0.0 {
                return;
            }
            let rate = |new: u64, old: u64| new.saturating_sub(old) as f64 / seconds;
            let cpus = f64::from(usage.vcpus.max(1));
            self.rates = Rates {
                cpu: (rate(usage.cpu_ns, last.cpu_ns) / 1e9 / cpus * 100.0).min(100.0),
                disk_read: rate(usage.disk_read, last.disk_read),
                disk_written: rate(usage.disk_written, last.disk_written),
                received: rate(usage.net_received, last.net_received),
                sent: rate(usage.net_sent, last.net_sent),
            };
            let r = self.rates;
            push(&mut self.cpu, r.cpu);
            push(&mut self.disk, r.disk_read + r.disk_written);
            push(&mut self.net, r.received + r.sent);
        }
        push(&mut self.memory, usage.memory_used_kib as f64 * 1024.0);
        self.last = Some((now, usage));
    }
}

fn push(series: &mut VecDeque<f64>, value: f64) {
    if series.len() == SAMPLES {
        series.pop_front();
    }
    series.push_back(value);
}

/// The Usage group for the machine `uuid`, with `history` carried over from the page
/// before if it is of the same machine.
pub fn group(
    view: &MachineView,
    uuid: &str,
    history: &Rc<RefCell<History>>,
) -> adw::PreferencesGroup {
    if history.borrow().uuid != uuid {
        history.replace(History {
            uuid: uuid.to_owned(),
            ..History::default()
        });
    }
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Usage"))
        .build();
    let row = |title: String, draw: fn(&History) -> (&VecDeque<f64>, f64)| {
        let graph = gtk::DrawingArea::builder()
            .content_width(160)
            .content_height(32)
            .valign(gtk::Align::Center)
            .build();
        let history = history.clone();
        graph.set_draw_func(move |area, cr, width, height| {
            let history = history.borrow();
            let (series, top) = draw(&history);
            plot(area, cr, series, top, f64::from(width), f64::from(height));
        });
        let row = adw::ActionRow::builder().title(title).build();
        row.add_suffix(&graph);
        group.add(&row);
        (row, graph)
    };
    let rows = [
        row(gettext("Processors"), |h| (&h.cpu, 100.0)),
        row(gettext("Memory"), |h| {
            let total = h.last.map_or(0, |(_, u)| u.memory_kib) as f64 * 1024.0;
            (&h.memory, total)
        }),
        row(gettext("Disks"), |h| (&h.disk, top(&h.disk, DISK_FLOOR))),
        row(gettext("Network"), |h| (&h.net, top(&h.net, NET_FLOOR))),
    ];
    let show = {
        let history = history.clone();
        move || {
            let h = history.borrow();
            let r = h.rates;
            let rate = |bytes: f64| gettext("{size}/s").replace("{size}", &size(bytes as u64));
            let (used, total) = h.last.map_or((0, 0), |(_, u)| {
                (u.memory_kib.min(u.memory_used_kib), u.memory_kib)
            });
            let texts = [
                format!("{:.0} %", r.cpu),
                gettext("{used} of {total}")
                    .replace("{used}", &size(used * 1024))
                    .replace("{total}", &size(total * 1024)),
                gettext("Read {read} · Written {written}")
                    .replace("{read}", &rate(r.disk_read))
                    .replace("{written}", &rate(r.disk_written)),
                gettext("Received {received} · Sent {sent}")
                    .replace("{received}", &rate(r.received))
                    .replace("{sent}", &rate(r.sent)),
            ];
            for ((row, graph), text) in rows.iter().zip(texts) {
                if h.last.is_some() {
                    row.set_subtitle(&text);
                }
                graph.queue_draw();
            }
        }
    };
    show();
    let poll = {
        let (group, view, history, uuid) = (
            group.downgrade(),
            view.downgrade(),
            history.clone(),
            uuid.to_owned(),
        );
        move || {
            let (Some(group), Some(win)) = (
                group.upgrade(),
                view.upgrade()
                    .and_then(|v| v.root())
                    .and_downcast::<MachinesWindow>(),
            ) else {
                return glib::ControlFlow::Break;
            };
            if !group.is_mapped() || history.borrow().polling {
                return glib::ControlFlow::Continue;
            }
            history.borrow_mut().polling = true;
            let (history, uuid, show) = (history.clone(), uuid.clone(), show.clone());
            glib::spawn_future_local(async move {
                let machine = uuid.clone();
                let usage = win.call(move |hv| hv.usage(&machine)).await;
                let mut h = history.borrow_mut();
                // The view went on to another machine meanwhile, whose history this is now.
                if h.uuid != uuid {
                    return;
                }
                h.polling = false;
                if let Some(Ok(usage)) = usage {
                    h.add(Instant::now(), usage);
                    drop(h);
                    show();
                }
            });
            glib::ControlFlow::Continue
        }
    };
    poll();
    glib::timeout_add_local(INTERVAL, poll);
    group
}

/// The top of a graph of `series`: its peak, or `floor` if that is higher.
fn top(series: &VecDeque<f64>, floor: f64) -> f64 {
    series.iter().copied().fold(floor, f64::max)
}

/// `series` as a filled line in the accent color, the latest sample at the right edge and
/// `top` at the top.
fn plot(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    series: &VecDeque<f64>,
    top: f64,
    width: f64,
    height: f64,
) {
    let accent = adw::StyleManager::for_display(&area.display()).accent_color_rgba();
    let (r, g, b) = (
        f64::from(accent.red()),
        f64::from(accent.green()),
        f64::from(accent.blue()),
    );
    let fg = area.color();
    cr.set_source_rgba(
        f64::from(fg.red()),
        f64::from(fg.green()),
        f64::from(fg.blue()),
        0.1,
    );
    cr.rectangle(0.0, 0.0, width, height);
    let _ = cr.fill();
    if series.len() < 2 || top <= 0.0 {
        return;
    }
    let step = width / (SAMPLES - 1) as f64;
    let x0 = width - step * (series.len() - 1) as f64;
    let y = |v: f64| height - (v / top).clamp(0.0, 1.0) * (height - 1.0);
    cr.move_to(x0, height);
    for (i, v) in series.iter().enumerate() {
        cr.line_to(x0 + step * i as f64, y(*v));
    }
    cr.line_to(width, height);
    cr.close_path();
    cr.set_source_rgba(r, g, b, 0.3);
    let _ = cr.fill();
    for (i, v) in series.iter().enumerate() {
        cr.line_to(x0 + step * i as f64, y(*v));
    }
    cr.set_source_rgb(r, g, b);
    cr.set_line_width(1.5);
    let _ = cr.stroke();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_come_from_two_samples() {
        let mut history = History::default();
        let start = Instant::now();
        let first = Usage {
            cpu_ns: 1_000_000_000,
            vcpus: 2,
            memory_used_kib: 1024,
            memory_kib: 4096,
            disk_read: 1000,
            net_sent: 500,
            ..Usage::default()
        };
        history.add(start, first);
        assert!(history.cpu.is_empty());
        assert_eq!(history.memory, [1024.0 * 1024.0]);
        history.add(
            start + Duration::from_secs(2),
            Usage {
                cpu_ns: 3_000_000_000,
                disk_read: 5000,
                net_sent: 1500,
                ..first
            },
        );
        assert_eq!(history.cpu, [50.0]);
        assert_eq!(history.disk, [2000.0]);
        assert_eq!(history.net, [500.0]);
        for i in 3..100 {
            history.add(start + Duration::from_secs(i), first);
        }
        assert_eq!(history.memory.len(), SAMPLES);
    }
}
