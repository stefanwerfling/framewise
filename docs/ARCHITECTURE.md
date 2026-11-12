# Framewise – Architektur

Framewise ist eine native Rust-Desktop-Anwendung (egui/eframe) zur Analyse und
Optimierung großer Plex-/Jellyfin-Videosammlungen. Dieses Dokument beschreibt
Architektur, Domain-Modell, Konzepte und den Implementierungsplan.

Leitprinzip: **erst korrekte Medienanalyse, dann sichere Verarbeitung, dann
Optimierung.** Originaldateien werden niemals verändert, verschoben oder
gelöscht.

---

## 1. Architekturübersicht

```
┌──────────────────────────── GUI-Thread (eframe) ────────────────────────────┐
│  gui::App  ── View-State (Filter, Sortierung, Auswahl, Formular-Entwürfe)   │
│     │  sendet Command                    ▲ empfängt AppEvent (non-blocking) │
└─────┼────────────────────────────────────┼──────────────────────────────────┘
      ▼                                    │
┌─────────────────────── Backend (Tokio-Runtime, eigener Thread) ─────────────┐
│ core::backend  ── Command-Dispatcher, hält Domain-State                     │
│   ├─ scanner      Discovery (walkdir) → ffprobe (begrenzt parallel)         │
│   ├─ media        ffprobe-JSON → Domain-Modell, Klassifikation, HDR         │
│   ├─ analyzer     Kompatibilität, Bitrate/BPP, Empfehlung, Einsparschätzung │
│   ├─ encoding     Profil → EncodePlan → ffmpeg-Argumente → Prozess          │
│   │                 Progress-Parser, Hardware-Scanner, Validierung, VMAF    │
│   ├─ queue        Job-Scheduler, Pause/Resume/Cancel, Recovery              │
│   ├─ monitor      sysinfo (CPU/RAM/Disk), nvidia-smi (optional)             │
│   └─ persistence  SQLite: Scans, Jobs, Historie, Profile                    │
└─────────────────────────────────────────────────────────────────────────────┘
          │ tokio::process::Command (Argumentliste, nie Shell-Strings)
          ▼
      ffmpeg / ffprobe (externe Prozesse)
```

* **GUI-State und Domain-State sind getrennt.** Die GUI hält nur Snapshots,
  die das Backend per `AppEvent` liefert, plus reinen View-State.
* **Kommunikation:** GUI → Backend über `tokio::sync::mpsc::UnboundedSender<Command>`
  (aus synchronem Code nutzbar), Backend → GUI über `std::sync::mpsc` +
  `egui::Context::request_repaint()`. Der GUI-Thread blockiert nie.
* **Fehler:** Domain-Fehler sind typisiert (eigener `Error`-Enum), tragen Kontext
  (Pfad, Kommando, stderr-Auszug) und werden als lesbare Meldung angezeigt.
* **Logging:** `tracing` mit Konsolen- und Datei-Ausgabe; vollständiges
  FFmpeg-stderr pro Job wird in `<data>/logs/jobs/<job-id>.log` geschrieben.

### Mehrsprachigkeit (Englisch / Deutsch)

* Quelltexte im Code sind ausschließlich **Englisch**:
  `tr!("Skip")`, `trf!("Audio track {}: …", n)`.
* Übersetzungen liegen in `locales/de.toml` (`"English" = "Deutsch"`), werden
  per `include_str!` eingebettet; fehlende Einträge fallen auf Englisch zurück.
* Platzhalter sind positionale `{}`; ein Unit-Test prüft, dass jeder
  `tr!`/`trf!`-Text übersetzt ist und die Platzhalteranzahl übereinstimmt.
* Sprache: Standard aus `LANG`, umschaltbar in den Einstellungen (sofort wirksam).
* Domain-Daten (Plan-Warnungen, Erkennungsgründe, Empfehlungen) werden als
  Enums gespeichert und erst bei der Anzeige übersetzt; in SQLite landen keine
  sprachabhängigen Texte. Ausnahme: Fehlermeldungen externer Tools.

## 2. Verzeichnisstruktur

```
framewise/
├─ Cargo.toml
├─ docs/ARCHITECTURE.md, CHANGELOG.md
├─ src/
│  ├─ main.rs                 Start: Logging, Config, Backend, eframe
│  ├─ lib.rs                  Modulbaum (für Integrationstests)
│  ├─ i18n.rs                 tr!/trf!, Sprachumschaltung
├─ locales/de.toml            Deutsche Übersetzungen (Englisch = Quelltext)
│  ├─ core/
│  │  ├─ config.rs            AppConfig (TOML), Defaults, Laden/Speichern
│  │  ├─ error.rs             Fehlertypen
│  │  ├─ events.rs            Command / AppEvent
│  │  ├─ paths.rs             Quelle/Ziel-Prüfung, Zielpfade, Kollisionen
│  │  ├─ backend.rs           Command-Dispatcher (Tokio)
│  │  └─ logging.rs
│  ├─ media/
│  │  ├─ model.rs             Domain-Modell (Video/Audio/Subtitle/HDR …)
│  │  ├─ probe.rs             ffprobe-Aufruf + JSON-Strukturen + Mapping
│  │  └─ classify.rs          Codec-Familien, HDR-, Atmos-, DTS:X-Erkennung
│  ├─ scanner/                Discovery, Symlink-Schutz, Duplikate, Ranking
│  ├─ analyzer/
│  │  ├─ resolution.rs        Zielauflösung (nie Upscaling, gerade Maße)
│  │  └─ recommend.rs         Kompatibilität, Potenzial, Empfehlung
│  ├─ encoding/
│  │  ├─ profile.rs           Profile A–D, Persistenz
│  │  ├─ plan.rs              EncodePlan: Stream-Entscheidungen + Warnungen
│  │  ├─ command.rs           ffmpeg-Argumentlisten
│  │  ├─ hardware.rs          Encoder/Decoder/Filter-Erkennung + Funktionstest
│  │  ├─ progress.rs          Parser für -progress pipe:1
│  │  ├─ process.rs           Prozessverwaltung (Nice, Pause, Abbruch)
│  │  ├─ validate.rs          Ergebnisvalidierung, Bericht
│  │  └─ quality.rs           VMAF/SSIM, CRF-Suche (Smart Encoding)
│  ├─ queue/                  Job-Modell, Scheduler, Recovery
│  ├─ monitor/                Ressourcenmonitor, Last-Gate
│  ├─ persistence/            SQLite-Repository
│  └─ gui/                    App, Theme, Views (Dashboard … Settings)
└─ tests/                     Integrationstests (erzeugen Testvideos per ffmpeg)
```

Bewusst **ein Crate** mit klaren Modulgrenzen statt Workspace: geringere
Komplexität, die Module sind trotzdem über `lib.rs` einzeln testbar.

## 3. Abhängigkeiten

| Crate | Zweck | Begründung |
|---|---|---|
| `eframe`, `egui_extras` | GUI, Tabellen | native, immediate-mode, keine Web-Runtime; `TableBuilder` für große Listen (virtualisiert) |
| `tokio` | Async-Runtime, Prozesse | `tokio::process` für nicht-blockierende ffmpeg/ffprobe-Aufrufe, Timer, Semaphoren |
| `serde`, `serde_json` | ffprobe-JSON, Persistenz | Standard; robuste `Option`/`default`-Felder |
| `toml` | Konfigurationsdatei | menschenlesbar und editierbar |
| `tracing`, `tracing-subscriber` | strukturiertes Logging | Spans pro Job, Filter per `RUST_LOG` |
| `sysinfo` | CPU/RAM/Disk | plattformübergreifend |
| `rusqlite` (bundled) | Persistenz | keine System-SQLite nötig; synchron, läuft in `spawn_blocking` |
| `walkdir` | rekursive Suche | Symlink-Loop-Erkennung eingebaut |
| `directories` | Config-/Daten-Pfade | XDG / AppData / Library korrekt |
| `rfd` | Ordnerauswahl-Dialog | nativer Dialog, läuft asynchron |
| `eframe` Feature `system_fonts` | Glyphen-Fallback | Symbole (→ ✔ ⟳ …), die die eingebetteten egui-Fonts nicht enthalten, kommen aus Systemschriften |
| `libc` (nur Unix) | `setpriority`, `SIGSTOP/SIGCONT` | Nice-Steuerung und echte Job-Pause |
| `tempfile` (dev) | Tests | temporäre Verzeichnisse |

Bewusst **nicht** verwendet: `thiserror`/`anyhow` (eigener Fehler-Enum mit lokalisiertem `Display`),
`fluent`/`rust-i18n` (siehe Abschnitt Mehrsprachigkeit),
`ffmpeg-next` (FFmpeg läuft laut Vorgabe als externer Prozess), `chrono`
(Unix-Zeitstempel genügen). `image` für Vorschaubilder ist für eine spätere
Ausbaustufe vorgesehen (Thumbnail per `ffmpeg -frames:v 1` → PNG → Texture).

## 4. Domain-Modell

```rust
struct MediaFile {
    id: i64, absolute_path, relative_path, file_name, extension,
    size_bytes: u64, modified_unix: i64,
    container: ContainerInfo { format_name, duration_s: Option<f64>,
                               bit_rate: Option<u64>, stream_count },
    video: Vec<VideoStream>, audio: Vec<AudioStream>, subtitles: Vec<SubtitleStream>,
    analysis: Option<Analysis>,          // Ergebnis der Recommendation Engine
}

struct VideoStream {
    index, codec: VideoCodec /* H264, Hevc, Av1, Vp9, Mpeg2, Mpeg4, Vc1, Other(String) */,
    profile: Option<String>, width, height, sar/dar: Option<Ratio>,
    frame_rate: Option<Ratio>, bit_rate: Option<u64>, pix_fmt: Option<String>,
    bit_depth: Option<u8>, color: ColorInfo { primaries, transfer, space, range },
    hdr: HdrFormat /* Sdr, Hdr10, Hdr10Plus, Hlg, DolbyVision{profile, compat_id, base} , Unknown */,
    is_attached_pic: bool,
}

struct AudioStream {
    index, type_index /* 0:a:N */, codec: AudioCodec /* Aac, Ac3, Eac3, Dts, DtsHd, TrueHd, Flac, Opus, Mp3, Pcm, Other */,
    profile, language: Option<String>, title, bit_rate, channels, channel_layout,
    sample_rate, default, forced,
    features: AudioFeatures { atmos: Detection, dts_x: Detection, dts_hd_ma: bool },
}

struct SubtitleStream {
    index, type_index, codec: SubtitleCodec /* Subrip, Ass, Ssa, WebVtt, MovText, Pgs, VobSub, DvbSub, Other */,
    kind: SubtitleKind /* Text | Bitmap | Unknown */, language, title,
    default, forced, hearing_impaired,
}
```

`Detection` unterscheidet `No`, `Probable(Reason)` (z. B. aus Track-Titel
„Atmos“) und `Confirmed(Reason)` (aus Codec-Profil, z. B. „Dolby TrueHD + Dolby
Atmos“). Unbekannte Werte sind immer `Option::None` bzw. `Other(String)` –
das Parsen schlägt nie wegen fehlender Metadaten fehl.

## 5. Scanner- und Queue-Konzept

### Scanner
1. **Validierung:** Quelle existiert, Ziel ≠ Quelle, Ziel nicht innerhalb der
   Quelle (kanonisierte Pfade) – sonst Abbruch mit Fehlermeldung.
2. **Discovery:** `walkdir` mit `follow_links(true)` (Loop-Erkennung durch
   walkdir), Endungen `.mkv`/`.mp4` case-insensitive, versteckte Dateien und
   Zielverzeichnis ausgeschlossen, Zugriffsfehler werden gesammelt und angezeigt.
3. **Analyse:** pro Datei `ffprobe -v error -print_format json -show_format
   -show_streams` (+ für PQ-Quellen ein Frame-Probe für HDR10+/DV-RPU),
   parallel begrenzt über `Semaphore` (Default: CPU-Kerne/2). Inkrementell:
   Dateien mit unveränderter Größe+mtime werden aus SQLite übernommen.
4. **Klassifikation & Empfehlung:** `analyzer` berechnet Kompatibilität,
   BPP, Einsparschätzung und Aktion.
5. **Duplikate:** Kandidaten über (Größe) bzw. (Dauer ±1 s, Auflösung,
   Dateistamm); bestätigt über Teil-Hash (erste/letzte 4 MiB + Größe).
6. Fortschritt per `AppEvent::ScanProgress`, Abbruch per `CancellationToken`
   (`Arc<AtomicBool>`).

### Encoding Queue
```
Queued ─▶ Preparing ─▶ QualitySearch ─▶ Encoding ─▶ Sidecars ─▶ Validating ─▶ Finalizing ─▶ Done
   ▲          │  (nur Smart-CRF)            │ ▲ pausiert (SIGSTOP)   │               │
   │          ├─▶ Skipped (Ziel existiert)   ▼                        ▼               ▼
   └─ retry ──┴──────── Failed ◀──── Cancelled                Failed (Quarantäne)
                  Interrupted (App beendet während der Job lief)
```
* Jobs werden in SQLite persistiert (Zustand, Plan, Fehler, Zeiten).
* **Scheduler:** startet Jobs bis `max_parallel_jobs`, sofern das
  **Last-Gate** frei ist (CPU-Last < Zielwert, freier RAM > Minimum, freier
  Platz im Arbeits-/Zielverzeichnis ausreichend). Sonst Verzögerung + Anzeige.
* **Arbeitsverzeichnis:** `<work>/<job-id>/` – ffmpeg schreibt nur dorthin. `<work>` ist standardmäßig `<Zielverzeichnis>/.framewise-work` (nicht die Systemplatte; Ergebnis wird per Umbenennen ins Ziel gelegt).
  Nach Validierung atomares `rename` in das Ziel; bei anderem Dateisystem
  zuerst Kopie in `<ziel-dir>/.framewise-<id>.partial`, dann `rename`.
  Existiert das Ziel inzwischen, wird **nicht** überschrieben (Konfliktstrategie).
* **Pause:** Queue-Pause (keine neuen Jobs) und Job-Pause (Unix: `SIGSTOP`/
  `SIGCONT`; Windows: nur Queue-Pause – ehrlich in der GUI ausgewiesen).
* **Abbruch:** Prozess beenden, Job-Arbeitsverzeichnis löschen (nur dieses).
* **Recovery:** Beim Start werden Jobs in einem aktiven Zustand auf
  `Interrupted` gesetzt und verwaiste Arbeitsverzeichnisse entfernt. Gibt es
  wartende Jobs, startet die Queue **pausiert** – der Benutzer entscheidet,
  wann es weitergeht.
* **Fortschritt:** ausschließlich über `-progress pipe:1`
  (`out_time_us`, `speed`, `fps`, `progress=end`) relativ zur Quelldauer.

### Validierung
Vor dem Verschieben ins Ziel wird das Ergebnis im Arbeitsverzeichnis geprüft:
Datei vorhanden und nicht leer, mit ffprobe lesbar, Videostream vorhanden,
Dauer innerhalb der Toleranz (max(1,5 s; 0,1 %) – bei 2 h also ≤ 7,2 s),
Auflösung, Anzahl und Sprachen der Audiospuren, Audio-Titel, Untertitel,
Sidecar-Dateien und optional ein vollständiger Decode-Test
(`-f null -`). Bei aktivierter Qualitätsprüfung zusätzlich SSIM/VMAF.
Schlägt eine Prüfung fehl, wird das Ergebnis (falls aktiviert) in
`<arbeitsverzeichnis>/quarantine/` aufbewahrt – das Ziel bleibt unberührt.

### Konfliktstrategien
`Skip` (Default) · `Rename` („Film (1).mp4“) · `ReplaceConfirmed` (nur nach
expliziter Bestätigung pro Job; das alte Ergebnis wird erst nach erfolgreicher
Validierung ersetzt). Quell-Dateien sind nie Ziel einer Operation.

## 6. FFmpeg-Profile

Gemeinsam: `-hide_banner -nostdin -n -progress pipe:1 -nostats`,
`-map_metadata 0 -map_chapters 0`, explizite `-map` je Stream,
`-movflags +faststart`, `-f mp4`, Sprache/Titel per `-metadata:s:X:N`,
Dispositionen per `-disposition:X:N`.

| | A Universal | B High Quality | C Space Saver | D Custom |
|---|---|---|---|---|
| Video | libx264 High@4.1, CRF 20, preset slow | libx264 CRF 18, preset slower | libx264 CRF 23 (oder libx265 CRF 24, opt.) | frei |
| Pixel | yuv420p 8 Bit | yuv420p | yuv420p (HEVC: yuv420p10 optional) | frei |
| Max. Auflösung | 1920×1080 | 1920×1080 | 1920×1080 | frei |
| Video-Copy erlaubt | wenn H.264 8 Bit ≤ Ziel & effizient | ja | nein (Ziel ist Größe) | frei |
| Audio | Compatibility | konfigurierbar | Compatibility | frei |
| HDR | Tonemapping → SDR BT.709 | Tonemapping | Tonemapping | frei |
| Qualitätsprüfung | optional | optional | **verpflichtend** (VMAF/SSIM) | frei |

**Skalierung:** `scale=W:H:flags=lanczos` mit vorberechneten geraden Maßen
(Seitenverhältnis inkl. SAR erhalten, nie Upscaling, kein Padding).

**Schwarze Balken** (`encoding/crop.rs`, Profiloption, Standard an): Erst
beim Start eines Jobs und nur, wenn die Videospur neu encodiert wird.
`cropdetect` läuft an 10 Stellen zwischen 10 % und 90 % des Films, jeweils
nur auf Keyframes. Abgeschnitten wird die **Vereinigung** aller erkannten
Bildbereiche, also nur, was an jeder Stelle schwarz ist. Dunkle Szenen
verkleinern das Bild dadurch nicht, und bei wechselndem Seitenverhältnis
bleibt das größere Bild, sobald eine Stelle es zeigt. Weitere Regeln: mindestens
die Hälfte der Stellen muss messbar sein, Balken unter 1 % (mind. 8 px) je
Achse bleiben, das Restbild muss mindestens 40 % je Achse behalten, Werte
werden nach außen auf gerade Zahlen gerundet. Der `crop`-Filter steht vor
der Skalierung; die Qualitätsreferenz nutzt dieselbe Filterkette. Balken
allein sind kein Grund, eine kopierbare Spur neu zu encodieren.

**HDR → SDR** (zscale/zimg, wenn verfügbar):
`zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p`
plus Ausgabe-Tags `-color_primaries bt709 -color_trc bt709 -colorspace bt709`.
Fehlt zscale, wird kein „stilles“ Metadaten-Entfernen gemacht – der Job wird
mit verständlicher Meldung blockiert. Dolby Vision Profil 5 (kein
HDR10-Basislayer) wird als nicht korrekt tonemapbar markiert.

**HDR erhalten** (Strategie „Preserve“, HEVC-Ziel): Eine passende Spur wird
kopiert. Muss neu encodiert werden, entsteht HEVC Main 10 mit libx265 auf der
CPU (Hardware-Encoder werden dafür übergangen):
* `-pix_fmt yuv420p10le`, Container-Tags `bt2020`/`smpte2084` bzw.
  `arib-std-b67`/`bt2020nc`/`tv`.
* `x265-params`: `colorprim`, `transfer`, `colormatrix`, `range`; bei PQ
  zusätzlich `hdr10=1:hdr10-opt=1`, `master-display` und `max-cll` aus der
  Quelle. Diese Werte liest der Runner erst beim Job-Start per ffprobe
  (Container-Side-Data vor den ersten Frames), damit Scan-Caches gültig bleiben.
* HDR10+ und Dolby Vision (Profil 7/8 mit HDR10- bzw. HLG-Basis) werden als
  HDR10 bzw. HLG erhalten, die dynamischen Metadaten entfallen (Hinweis im
  Plan). Dolby Vision Profil 5 und unbekannte Formate werden tonemappt.
* `-dolbyvision 0`, wenn libx265 die Option kennt (ffmpeg ≥ 7, Standard
  „auto“): Das Ergebnis gibt nie Dolby Vision vor.
* Kann libx265 kein 10 Bit, wird tonemappt (Hinweis im Plan).
* Die Validierung prüft den Dynamikumfang: HDR10/HLG in 10 Bit beim Erhalt,
  sonst SDR.
* Profil E sucht in diesem Fall nur HEVC, weil H.264 kein HDR tragen kann.

**10 Bit für SDR** (Profiloption, Standard an): HEVC-Encodes auf der CPU
laufen in 10 Bit (weniger Banding). Hardware-Encoder bleiben bei SDR auf 8 Bit,
H.264 immer. Referenz und Messung der Qualitätssuche nutzen dasselbe Format.

**Audio (Compatibility):** AAC/AC3/E-AC3 → copy; DTS/DTS-HD/TrueHD/FLAC/PCM
mit >2 Kanälen → kompatible Mehrkanalspur (pro Profil wählbar: AC3 5.1 –
Standard in A/C –, E-AC3 bis 7.1 – Standard in B – oder AAC)
(mit Hinweis „Atmos/DTS:X-Objekte gehen verloren“); zusätzlich eine
AAC-LC-Stereo-Spur je Sprache, falls keine existiert. **Preserve Original:**
alles kopieren, was MP4 aufnehmen kann; nicht aufnehmbare Spuren werden
konvertiert und **vor** dem Job als Warnung angezeigt. Keine Spur entfällt
ohne Eintrag im Plan.

**Untertitel:** Text (SRT/ASS/SSA/WebVTT/mov_text) → `mov_text`
(ASS-Styling-Verlust wird angezeigt). Bitmap (PGS/VobSub/DVB) → werden als
Sidecar-Dateien neben dem Ergebnis exportiert (`Film.ger.forced.sup`,
VobSub als `Film.ger.mks`), Warnung in der GUI. OCR ist als spätere
Ausbaustufe vorgesehen.

**MP4-Eigenheiten (per ffprobe-Experiment verifiziert):**
* Spurtitel landen im MP4 nur als `handler_name`. Framewise schreibt Titel
  deshalb zusätzlich als `handler_name` und liest ihn beim Scannen zurück
  (generische Werte wie „SoundHandler“ werden ignoriert). Die Validierung
  prüft die Audio-Titel.
* Das *forced*-Flag kann ffmpeg im MP4 nicht speichern. Erzwungene
  Text-Untertitel erhalten daher den Titel-Marker „Forced“ und zusätzlich
  eine Sidecar-Datei `Film.<lang>.forced.srt`, die Player zuverlässig
  erkennen. Der Plan zeigt das als Hinweis an (`ForcedFlagViaSidecar`).
* Genau eine Audiospur ist `default`; alle anderen Dispositionen werden
  explizit gesetzt, damit keine Quell-Flags „durchsickern“.

**Hardware:** `h264_nvenc` (`-preset p5 -rc vbr -cq N -b:v 0`),
`h264_qsv` (`-global_quality N`), `h264_vaapi` (`-vaapi_device …
-vf …,format=nv12,hwupload -qp N`), `h264_amf` (`-rc cqp -qp_i/-qp_p N`),
`h264_videotoolbox` (`-b:v` aus Schätzung). Verfügbarkeit wird per
`-encoders` **und** einem 1-Frame-Funktionstest geprüft. Hardware-Decoding
(`-hwaccel …`) ist separat schaltbar; bei Fehlern automatischer CPU-Fallback.

## 7. Implementierungsplan

| Phase | Inhalt | Ergebnis |
|---|---|---|
| 1 Foundation | Cargo-Projekt, Config, Logging, ffmpeg/ffprobe-Erkennung, GUI-Grundgerüst, Backend-Thread | App startet, zeigt Tools/Version |
| 2 Scanner | Domain-Modell, ffprobe-Parsing, Discovery, Analyzer, Media Library (Sortierung/Filter/Details), SQLite-Cache | Vollständige Medienanalyse |
| 3 Encoding | Profile, EncodePlan, Argumentbau, Prozessmanager, Progress, Validierung, atomares Finalisieren | Sichere CPU-Konvertierung |
| 4 GPU | Hardware-Scanner, NVENC/QSV/VAAPI/AMF/VT, Fallback | Optionale Beschleunigung |
| 5 Queue/Ressourcen | Scheduler, Parallelität, Last-Gate, Pause/Resume, Recovery, Monitor | Robuster Batchbetrieb |
| 6 Qualität | HDR-Tonemapping, SSIM/VMAF, CRF-Suche | Smart Encoding |
| 7 Stabilisierung | Tests, Clippy, Doku, Release-Profil | Release-fähig |

Nach jeder Phase: `cargo test`, `cargo clippy --all-targets -- -D warnings`,
`cargo fmt --check`; Änderungen in `docs/CHANGELOG.md`.
