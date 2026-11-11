//! Hardware monitor: capabilities and live resource usage.

use eframe::egui::{self, RichText};

use crate::core::events::Command;
use crate::encoding::hardware::encoder_name;
use crate::encoding::profile::TargetVideoCodec;
use crate::gui::{FramewiseApp, format, theme};
use crate::{tr, trf};

fn bar(ui: &mut egui::Ui, label: &str, fraction: f32, text: String) {
    ui.horizontal(|ui| {
        ui.add_sized([120.0, 18.0], egui::Label::new(label));
        let color = if fraction > 0.9 {
            theme::ERROR
        } else if fraction > 0.7 {
            theme::WARN
        } else {
            theme::ACCENT
        };
        ui.add(
            egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                .desired_width(320.0)
                .fill(color)
                .text(RichText::new(text).color(egui::Color32::WHITE)),
        );
    });
}

fn sparkline(ui: &mut egui::Ui, values: &[f32]) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().min(480.0), 60.0),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, egui::Color32::from_rgb(18, 19, 22));
    if values.len() < 2 {
        return;
    }
    let n = values.len() as f32;
    let points: Vec<egui::Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            egui::pos2(
                rect.left() + rect.width() * i as f32 / (n - 1.0),
                rect.bottom() - rect.height() * (v / 100.0).clamp(0.0, 1.0),
            )
        })
        .collect();
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(1.5, theme::ACCENT),
    ));
}

pub fn show(app: &mut FramewiseApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.heading(tr!("Hardware monitor"));
        if ui.button(tr!("⟳ Re-detect hardware")).clicked() {
            app.send(Command::RescanHardware);
        }
    });
    ui.separator();
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.columns(2, |cols| {
            let ui = &mut cols[0];
            let r = &app.snap.resources;
            theme::card().show(ui, |ui| {
                ui.label(RichText::new(tr!("Live usage")).strong());
                ui.label(RichText::new(&r.cpu_brand).small().color(theme::MUTED));
                bar(ui, "CPU", r.cpu_percent / 100.0, format!("{:.0} %", r.cpu_percent));
                let history: Vec<f32> = app.snap.cpu_history.iter().copied().collect();
                sparkline(ui, &history);
                if let Some(l) = r.load_avg {
                    ui.label(RichText::new(format!("Load: {:.2} / {:.2} / {:.2}", l[0], l[1], l[2])).small());
                }
                if r.mem_total > 0 {
                    let used = r.mem_total.saturating_sub(r.mem_available);
                    bar(
                        ui,
                        "RAM",
                        used as f32 / r.mem_total as f32,
                        format!("{} / {}", format::bytes(used), format::bytes(r.mem_total)),
                    );
                }
                if r.swap_total > 0 {
                    bar(
                        ui,
                        "Swap",
                        r.swap_used as f32 / r.swap_total as f32,
                        format!("{} / {}", format::bytes(r.swap_used), format::bytes(r.swap_total)),
                    );
                }
                ui.collapsing(trf!("{} CPU threads", r.per_core.len()), |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for (i, c) in r.per_core.iter().enumerate() {
                            ui.add(
                                egui::ProgressBar::new(c / 100.0)
                                    .desired_width(70.0)
                                    .text(format!("{i}: {c:.0}%")),
                            );
                        }
                    });
                });
                for g in &r.gpus {
                    ui.separator();
                    ui.label(RichText::new(&g.name).strong());
                    if let Some(u) = g.utilization {
                        bar(ui, "GPU", u / 100.0, format!("{u:.0} %"));
                    }
                    if let Some(u) = g.encoder_utilization {
                        bar(ui, tr!("Encoder"), u / 100.0, format!("{u:.0} %"));
                    }
                    if let Some(u) = g.decoder_utilization {
                        bar(ui, tr!("Decoder"), u / 100.0, format!("{u:.0} %"));
                    }
                    if let (Some(used), Some(total)) = (g.memory_used_mb, g.memory_total_mb) {
                        bar(ui, "VRAM", used as f32 / total.max(1) as f32, format!("{used} / {total} MB"));
                    }
                    if let Some(t) = g.temperature_c {
                        ui.label(format!("{t:.0} °C"));
                    }
                }
                if r.gpus.is_empty() {
                    ui.label(RichText::new(tr!("No GPU utilisation available (only NVIDIA via nvidia-smi is supported)")).small().color(theme::MUTED));
                }
            });
            theme::card().show(ui, |ui| {
                ui.label(RichText::new(tr!("Disks")).strong());
                for d in r.disks.iter().filter(|d| d.total > 0) {
                    let used = d.total.saturating_sub(d.available);
                    bar(
                        ui,
                        &d.mount_point.display().to_string(),
                        used as f32 / d.total as f32,
                        trf!("{} free", format::bytes(d.available)),
                    );
                    if d.read_bps > 0 || d.write_bps > 0 {
                        ui.label(RichText::new(format!(
                            "    R {}/s · W {}/s",
                            format::bytes(d.read_bps),
                            format::bytes(d.write_bps)
                        )).small().color(theme::MUTED));
                    }
                }
            });

            let ui = &mut cols[1];
            theme::card().show(ui, |ui| {
                ui.label(RichText::new(tr!("FFmpeg")).strong());
                match &app.snap.tools {
                    Some(t) => {
                        ui.label(t.ffmpeg_version.clone().or(t.ffmpeg_error.clone()).unwrap_or_default());
                        ui.label(t.ffprobe_version.clone().or(t.ffprobe_error.clone()).unwrap_or_default());
                    }
                    None => {
                        ui.spinner();
                    }
                }
            });
            theme::card().show(ui, |ui| {
                ui.label(RichText::new(tr!("Capabilities")).strong());
                let Some(caps) = app.snap.caps.clone() else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(tr!("Testing encoders…"));
                    });
                    return;
                };
                ui.label(RichText::new(tr!("GPU models")).color(theme::MUTED));
                if caps.gpus.is_empty() {
                    ui.label(tr!("none detected"));
                }
                for g in &caps.gpus {
                    ui.label(format!("{}  ({})", g.name, g.source));
                }
                if !caps.render_devices.is_empty() {
                    ui.label(RichText::new(format!(
                        "DRM: {}",
                        caps.render_devices.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                    )).small());
                }
                ui.add_space(4.0);
                ui.label(RichText::new(tr!("Hardware encoders (functional test)")).color(theme::MUTED));
                if caps.tests.is_empty() {
                    ui.label(tr!("No hardware encoder offered by this ffmpeg build – CPU encoding is used."));
                }
                egui::Grid::new("enc_tests").striped(true).num_columns(3).show(ui, |ui| {
                    for t in &caps.tests {
                        ui.label(if t.works {
                            RichText::new("✔").color(theme::OK)
                        } else {
                            RichText::new("✖").color(theme::ERROR)
                        });
                        ui.label(&t.encoder);
                        // first line only; the full ffmpeg output is available on hover
                        let summary = t
                            .error
                            .as_deref()
                            .map_or_else(|| tr!("works").to_string(), |e| e.lines().next().unwrap_or_default().to_string());
                        ui.add(egui::Label::new(RichText::new(summary).small()).truncate())
                            .on_hover_text(t.error.clone().unwrap_or_default());
                        ui.end_row();
                    }
                });
                ui.add_space(4.0);
                ui.label(RichText::new(tr!("CPU encoders")).color(theme::MUTED));
                for codec in [TargetVideoCodec::H264, TargetVideoCodec::Hevc] {
                    let name = encoder_name(crate::core::config::HwBackend::Cpu, codec).unwrap_or_default();
                    let ok = caps.encoders.contains(name);
                    ui.label(RichText::new(format!("{} {name}", if ok { "✔" } else { "✖" })).color(if ok { theme::OK } else { theme::ERROR }));
                }
                ui.add_space(4.0);
                ui.label(RichText::new(tr!("Hardware decoders (hwaccel)")).color(theme::MUTED));
                ui.label(if caps.hwaccels.is_empty() { "–".into() } else { caps.hwaccels.join(", ") });
                ui.add_space(4.0);
                ui.label(RichText::new(tr!("Relevant filters")).color(theme::MUTED));
                ui.horizontal_wrapped(|ui| {
                    for f in ["zscale", "tonemap", "libvmaf", "ssim", "psnr", "bwdif", "scale_cuda", "scale_vaapi", "vpp_qsv", "tonemap_opencl"] {
                        let ok = caps.has_filter(f);
                        ui.label(RichText::new(format!("{} {f}", if ok { "✔" } else { "✖" })).small().color(if ok { theme::OK } else { theme::MUTED }));
                    }
                });
            });
            theme::card().show(ui, |ui| {
                ui.label(RichText::new(tr!("Limits")).strong());
                ui.label(RichText::new(tr!("A GPU usage limit is not offered because it cannot be enforced reliably. CPU load is a target value: new jobs are delayed while it is exceeded; running ffmpeg processes get a lower priority (nice).")).small().color(theme::MUTED));
            });
        });
    });
}
