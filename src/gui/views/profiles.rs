//! Encoding profile editor (profiles A–D).

use eframe::egui::{self, RichText};

use crate::core::events::Command;
use crate::encoding::profile::{
    AudioStrategy, BitmapSubtitleAction, CompatStereoMode, EncodingProfile, HdrStrategy,
    MultichannelCodec, ProfileKind, QualityMetric, TargetVideoCodec, TonemapAlgorithm,
};
use crate::gui::{FramewiseApp, theme};
use crate::{tr, trf};

#[derive(Default)]
pub struct ProfilesUi {
    selected: Option<String>,
    draft: Option<EncodingProfile>,
}

impl ProfilesUi {
    pub fn on_profiles_changed(&mut self, profiles: &[EncodingProfile]) {
        if let Some(id) = &self.selected
            && !profiles.iter().any(|p| &p.id == id)
        {
            self.selected = None;
            self.draft = None;
        }
    }
}

const PRESETS: [&str; 9] = [
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
    "slow",
    "slower",
    "veryslow",
];

fn row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.label(label);
    // a grid puts every widget into its own cell – keep the value widgets together
    ui.horizontal_wrapped(|ui| add(ui));
    ui.end_row();
}

pub fn show(app: &mut FramewiseApp, ui: &mut egui::Ui) {
    ui.heading(tr!("Encoding profiles"));
    ui.label(
        RichText::new(tr!("Profiles are stored permanently. Built-in profiles can be adjusted and reset; 'Duplicate' creates a custom profile (D)."))
            .color(theme::MUTED),
    );
    ui.separator();

    let profiles = app.snap.profiles.clone();
    if app.profiles_ui.selected.is_none() {
        app.profiles_ui.selected = Some(app.snap.config.encoding.active_profile.clone());
    }

    egui::Panel::left("profile_list")
        .resizable(false)
        .exact_size(240.0)
        .show(ui, |ui| {
            for p in &profiles {
                let active = p.id == app.snap.config.encoding.active_profile;
                let text = format!("{}{}", if active { "★ " } else { "" }, p.name);
                if ui
                    .add_sized(
                        [ui.available_width(), 24.0],
                        egui::Button::selectable(
                            app.profiles_ui.selected.as_ref() == Some(&p.id),
                            text,
                        ),
                    )
                    .on_hover_text(p.kind.label())
                    .clicked()
                {
                    app.profiles_ui.selected = Some(p.id.clone());
                    app.profiles_ui.draft = None;
                }
            }
            ui.add_space(8.0);
            if ui.button(tr!("+ New custom profile")).clicked() {
                let id = format!("custom_{}", crate::queue::job::now_unix());
                let p = EncodingProfile::custom(id.clone(), tr!("Custom profile"));
                app.send(Command::SaveProfile(Box::new(p)));
                app.profiles_ui.selected = Some(id);
                app.profiles_ui.draft = None;
            }
        });

    let Some(selected) = profiles
        .iter()
        .find(|p| Some(&p.id) == app.profiles_ui.selected.as_ref())
        .cloned()
    else {
        ui.label(tr!("Select a profile"));
        return;
    };
    let draft = app
        .profiles_ui
        .draft
        .get_or_insert_with(|| selected.clone());
    if draft.id != selected.id {
        *draft = selected.clone();
    }
    let dirty = *draft != selected;

    let mut save = false;
    let mut duplicate = false;
    let mut delete = false;
    let mut activate = false;
    let mut revert = false;

    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(tr!("Name"));
            ui.text_edit_singleline(&mut draft.name);
            ui.label(RichText::new(draft.kind.label()).color(theme::MUTED));
            if draft.builtin {
                ui.label(RichText::new(tr!("built-in")).small().color(theme::MUTED));
            }
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(dirty, egui::Button::new(RichText::new(tr!("Save")).strong())).clicked() {
                save = true;
            }
            if ui.add_enabled(dirty, egui::Button::new(tr!("Discard changes"))).clicked() {
                revert = true;
            }
            if ui.button(tr!("Duplicate")).clicked() {
                duplicate = true;
            }
            let is_active = draft.id == app.snap.config.encoding.active_profile;
            if ui.add_enabled(!is_active, egui::Button::new(tr!("Use as active profile"))).clicked() {
                activate = true;
            }
            let label = if draft.builtin { tr!("Reset to defaults") } else { tr!("Delete") };
            if ui.button(label).clicked() {
                delete = true;
            }
        });
        ui.separator();

        ui.columns(2, |cols| {
            // ---------------- video
            let ui = &mut cols[0];
            theme::card().show(ui, |ui| {
                ui.label(RichText::new(tr!("Video")).strong());
                egui::Grid::new("video_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    let v = &mut draft.video;
                    row(ui, tr!("Target codec"), |ui| {
                        egui::ComboBox::from_id_salt("codec")
                            .selected_text(v.codec.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut v.codec, TargetVideoCodec::H264, TargetVideoCodec::H264.label());
                                ui.selectable_value(&mut v.codec, TargetVideoCodec::Hevc, TargetVideoCodec::Hevc.label())
                                    .on_hover_text(tr!("HEVC is not supported by every client"));
                            });
                    });
                    row(ui, tr!("Quality (CRF)"), |ui| {
                        ui.add(egui::Slider::new(&mut v.crf, 14..=32))
                            .on_hover_text(tr!("Lower = better quality and larger files. 18–22 is visually transparent for most content."));
                    });
                    row(ui, tr!("Encoder preset"), |ui| {
                        egui::ComboBox::from_id_salt("preset")
                            .selected_text(v.preset.clone())
                            .show_ui(ui, |ui| {
                                for p in PRESETS {
                                    ui.selectable_value(&mut v.preset, p.to_string(), p);
                                }
                            });
                    });
                    row(ui, tr!("Max. resolution"), |ui| {
                        ui.add(egui::DragValue::new(&mut v.max_width).range(320..=7680));
                        ui.label("×");
                        ui.add(egui::DragValue::new(&mut v.max_height).range(240..=4320));
                        for (w, h) in [(1920, 1080), (1280, 720), (3840, 2160)] {
                            if ui.small_button(format!("{h}p")).clicked() {
                                v.max_width = w;
                                v.max_height = h;
                            }
                        }
                    });
                    row(ui, tr!("H.264 profile / level"), |ui| {
                        ui.add(egui::TextEdit::singleline(&mut v.h264_profile).desired_width(60.0));
                        ui.add(egui::TextEdit::singleline(&mut v.h264_level).desired_width(40.0));
                    });
                    row(ui, tr!("Copy compatible video"), |ui| {
                        ui.checkbox(&mut v.allow_copy, "")
                            .on_hover_text(tr!("Already compatible and efficient video streams are not re-encoded"));
                        ui.label(tr!("up to"));
                        ui.add(egui::DragValue::new(&mut v.copy_max_bpp).range(0.02..=1.0).speed(0.01));
                        ui.label(tr!("bit/pixel"));
                    });
                    row(ui, tr!("Deinterlace"), |ui| {
                        ui.checkbox(&mut v.deinterlace, tr!("only frames flagged as interlaced"));
                    });
                    row(ui, "Tune", |ui| {
                        let mut tune = v.tune.clone().unwrap_or_default();
                        egui::ComboBox::from_id_salt("tune")
                            .selected_text(if tune.is_empty() { tr!("none") } else { tune.as_str() })
                            .show_ui(ui, |ui| {
                                for t in ["", "film", "animation", "grain"] {
                                    ui.selectable_value(&mut tune, t.to_string(), if t.is_empty() { tr!("none") } else { t });
                                }
                            });
                        v.tune = (!tune.is_empty()).then_some(tune);
                    });
                });
            });

            theme::card().show(ui, |ui| {
                ui.label(RichText::new("HDR").strong());
                egui::Grid::new("hdr_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    let h = &mut draft.hdr;
                    row(ui, tr!("Strategy"), |ui| {
                        egui::ComboBox::from_id_salt("hdr_strategy")
                            .selected_text(h.strategy.label())
                            .show_ui(ui, |ui| {
                                for s in [HdrStrategy::TonemapToSdr, HdrStrategy::Preserve] {
                                    ui.selectable_value(&mut h.strategy, s, s.label());
                                }
                            });
                    });
                    row(ui, tr!("Tone mapping"), |ui| {
                        egui::ComboBox::from_id_salt("tonemap")
                            .selected_text(h.algorithm.ffmpeg_name())
                            .show_ui(ui, |ui| {
                                for a in [TonemapAlgorithm::Hable, TonemapAlgorithm::Mobius, TonemapAlgorithm::Reinhard] {
                                    ui.selectable_value(&mut h.algorithm, a, a.ffmpeg_name());
                                }
                            });
                    });
                    row(ui, tr!("Highlight desaturation"), |ui| {
                        ui.add(egui::Slider::new(&mut h.desat, 0.0..=2.0));
                    });
                    row(ui, tr!("Nominal peak (nits)"), |ui| {
                        ui.add(egui::DragValue::new(&mut h.npl).range(50..=400));
                    });
                });
            });

            // ---------------- audio / subtitles / quality
            let ui = &mut cols[1];
            theme::card().show(ui, |ui| {
                ui.label(RichText::new(tr!("Audio")).strong());
                egui::Grid::new("audio_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    let a = &mut draft.audio;
                    row(ui, tr!("Strategy"), |ui| {
                        egui::ComboBox::from_id_salt("audio_strategy")
                            .selected_text(a.strategy.label())
                            .show_ui(ui, |ui| {
                                for s in [AudioStrategy::Compatibility, AudioStrategy::PreserveOriginal] {
                                    ui.selectable_value(&mut a.strategy, s, s.label());
                                }
                            });
                    });
                    row(ui, tr!("AAC stereo track"), |ui| {
                        egui::ComboBox::from_id_salt("compat_stereo")
                            .selected_text(a.compat_stereo.label())
                            .show_ui(ui, |ui| {
                                for m in [CompatStereoMode::PerLanguage, CompatStereoMode::PrimaryLanguage, CompatStereoMode::Never] {
                                    ui.selectable_value(&mut a.compat_stereo, m, m.label());
                                }
                            });
                        ui.add(egui::DragValue::new(&mut a.stereo_bitrate_k).range(96..=320).suffix(" kb/s"));
                    });
                    row(ui, tr!("Multichannel conversion"), |ui| {
                        egui::ComboBox::from_id_salt("mc_codec")
                            .selected_text(a.multichannel_codec.label())
                            .show_ui(ui, |ui| {
                                for c in [MultichannelCodec::Ac3, MultichannelCodec::Eac3, MultichannelCodec::Aac] {
                                    ui.selectable_value(&mut a.multichannel_codec, c, c.label());
                                }
                            });
                        ui.add(egui::DragValue::new(&mut a.multichannel_bitrate_k).range(192..=1536).suffix(" kb/s"));
                    });
                    row(ui, tr!("Default track"), |ui| {
                        ui.checkbox(&mut a.compat_stereo_default, tr!("AAC stereo track instead of original"));
                    });
                });
                ui.label(
                    RichText::new(tr!("Dolby Atmos / DTS:X objects cannot be preserved by a conversion; affected tracks are listed before encoding."))
                        .small()
                        .color(theme::MUTED),
                );
            });

            theme::card().show(ui, |ui| {
                ui.label(RichText::new(tr!("Subtitles")).strong());
                egui::Grid::new("sub_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    let s = &mut draft.subtitles;
                    row(ui, tr!("Text subtitles"), |ui| {
                        ui.label("→ mov_text");
                        ui.checkbox(&mut s.text_sidecar, tr!("additionally as .srt"));
                    });
                    row(ui, tr!("Bitmap (PGS/VobSub)"), |ui| {
                        egui::ComboBox::from_id_salt("bitmap")
                            .selected_text(s.bitmap.label())
                            .show_ui(ui, |ui| {
                                for b in [BitmapSubtitleAction::Sidecar, BitmapSubtitleAction::Skip] {
                                    ui.selectable_value(&mut s.bitmap, b, b.label());
                                }
                            });
                    });
                });
                ui.label(RichText::new(tr!("OCR conversion of bitmap subtitles is planned for a later version.")).small().color(theme::MUTED));
            });

            theme::card().show(ui, |ui| {
                ui.label(RichText::new(tr!("Quality / smart encoding")).strong());
                egui::Grid::new("q_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    let q = &mut draft.quality;
                    row(ui, tr!("Metric"), |ui| {
                        egui::ComboBox::from_id_salt("metric")
                            .selected_text(q.metric.label())
                            .show_ui(ui, |ui| {
                                for m in [QualityMetric::Off, QualityMetric::Ssim, QualityMetric::Vmaf] {
                                    ui.selectable_value(&mut q.metric, m, m.label());
                                }
                            });
                    });
                    row(ui, tr!("Minimum quality"), |ui| {
                        ui.label("VMAF");
                        ui.add(egui::DragValue::new(&mut q.min_vmaf).range(70.0..=100.0).speed(0.1));
                        ui.label("SSIM");
                        ui.add(egui::DragValue::new(&mut q.min_ssim).range(0.9..=1.0).speed(0.001).max_decimals(4));
                    });
                    row(ui, tr!("Smart CRF search"), |ui| {
                        ui.checkbox(&mut q.smart_crf, "");
                        ui.label("CRF");
                        ui.add(egui::DragValue::new(&mut q.crf_min).range(10..=40));
                        ui.label("–");
                        ui.add(egui::DragValue::new(&mut q.crf_max).range(10..=40));
                    });
                    row(ui, tr!("Samples"), |ui| {
                        ui.add(egui::DragValue::new(&mut q.sample_count).range(1..=16));
                        ui.label("×");
                        ui.add(egui::DragValue::new(&mut q.sample_seconds).range(5..=120).suffix(" s"));
                    });
                    row(ui, tr!("Sample selection"), |ui| {
                        ui.checkbox(&mut q.scene_windows, tr!("hardest and typical scenes"));
                    });
                    row(ui, tr!("Quality floor"), |ui| {
                        ui.checkbox(&mut q.judge_min, tr!("applies to the weakest sample"));
                    });
                    row(ui, tr!("Compare HEVC"), |ui| {
                        ui.checkbox(&mut q.compare_hevc, "");
                        ui.add_enabled_ui(q.compare_hevc, |ui| {
                            egui::ComboBox::from_id_salt("hevc_preset")
                                .selected_text(q.hevc_preset.clone())
                                .show_ui(ui, |ui| {
                                    for p in ["fast", "medium", "slow", "slower"] {
                                        ui.selectable_value(&mut q.hevc_preset, p.to_string(), p);
                                    }
                                });
                        });
                    });
                    row(ui, tr!("HEVC only if smaller by"), |ui| {
                        ui.add_enabled_ui(q.compare_hevc, |ui| {
                            let mut pct = q.hevc_min_saving * 100.0;
                            if ui.add(egui::DragValue::new(&mut pct).range(0.0..=60.0).suffix(" %")).changed() {
                                q.hevc_min_saving = pct / 100.0;
                            }
                        });
                    });
                    row(ui, tr!("Copy video if saving below"), |ui| {
                        let mut pct = q.min_saving * 100.0;
                        if ui.add(egui::DragValue::new(&mut pct).range(0.0..=50.0).suffix(" %")).changed() {
                            q.min_saving = pct / 100.0;
                        }
                        ui.label(RichText::new(tr!("0 = always re-encode")).small().color(theme::MUTED));
                    });
                    row(ui, tr!("Analysis budget"), |ui| {
                        ui.add(egui::DragValue::new(&mut q.analysis_budget_min).range(5..=240).suffix(" min"));
                    });
                    row(ui, tr!("Mandatory"), |ui| {
                        let forced = draft.kind == ProfileKind::SpaceSaver;
                        ui.add_enabled(!forced, egui::Checkbox::new(&mut q.mandatory, tr!("fail job below minimum")));
                        if forced {
                            ui.label(RichText::new(tr!("always for Space Saver")).small().color(theme::MUTED));
                        }
                    });
                });
                if draft.quality.metric == QualityMetric::Vmaf
                    && !app.snap.caps.as_ref().is_some_and(|c| c.has_vmaf())
                {
                    ui.label(RichText::new(tr!("This ffmpeg build has no libvmaf – SSIM will be used instead.")).small().color(theme::WARN));
                }
                ui.label(RichText::new(tr!("A smaller file is never preferred over the minimum quality. Measurements cost additional time.")).small().color(theme::MUTED));
                if draft.quality.compare_hevc {
                    ui.label(RichText::new(tr!("H.264 stays the default and the fallback. HEVC (tagged hvc1) is only used when it is clearly smaller at the same quality; older devices and some browsers cannot play it.")).small().color(theme::MUTED));
                }
            });
        });
    });

    if revert {
        app.profiles_ui.draft = None;
    }
    if save && let Some(d) = app.profiles_ui.draft.clone() {
        app.send(Command::SaveProfile(Box::new(d)));
    }
    if duplicate && let Some(d) = app.profiles_ui.draft.clone() {
        let id = format!("custom_{}", crate::queue::job::now_unix());
        let mut copy = d;
        copy.id = id.clone();
        copy.name = trf!("{} (copy)", copy.name);
        copy.kind = ProfileKind::Custom;
        copy.builtin = false;
        app.send(Command::SaveProfile(Box::new(copy)));
        app.profiles_ui.selected = Some(id);
        app.profiles_ui.draft = None;
    }
    if activate {
        let id = selected.id.clone();
        app.update_config(|c| c.encoding.active_profile = id);
    }
    if delete {
        app.send(Command::DeleteProfile(selected.id.clone()));
        app.profiles_ui.draft = None;
    }
}
