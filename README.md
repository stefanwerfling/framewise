# Framewise – Media Optimizer

Native Desktop-Anwendung (Rust, egui), die eine Videobibliothek (MKV/MP4) analysiert und
Dateien kontrolliert in kompatible, platzsparende MP4s umwandelt – ohne Audio-, Untertitel-
oder Videoinformationen stillschweigend zu verlieren.

**Grundsatz:** Originaldateien werden niemals verändert, gelöscht oder überschrieben.
Ergebnisse entstehen im Arbeitsverzeichnis und werden erst nach erfolgreicher Validierung
in das Zielverzeichnis gelegt (gespiegelte Ordnerstruktur).

## Funktionen
- Rekursiver Scan mit vollständigen Metadaten: HDR10/HDR10+/HLG/Dolby Vision, Atmos/DTS:X,
  alle Audio- und Untertitelspuren.
- Empfehlungen (neu encodieren, Remux, bereits optimal, prüfen) mit geschätzter Einsparung,
  Top-N je Ordner und Duplikaterkennung.
- Profile A Universal, B High Quality, C Space Saver, D Custom und E Automatisch;
  max. 1080p, kein Upscaling.
- Profil E gibt das Qualitätsziel vor, nicht das Verfahren (Standard: VMAF ≥ 95 an der
  schwächsten Probe, Analysebudget 30 min). Pro Film:
  - Proben an den aufwendigsten Stellen, ermittelt aus einer Komplexitätskarte
    (Paketgrößen per ffprobe, ohne Decodieren)
  - CRF-Suche für H.264 und, falls libx265 vorhanden ist, für HEVC (`hvc1`); HEVC nur bei
    mindestens 15 % Ersparnis
  - Videospur kopieren, wenn kein Verfahren mindestens 10 % spart
  - Die Entscheidung wird im Job gespeichert und in Queue und Historie angezeigt.
- HDR → SDR-Tonemapping; Smart CRF über SSIM bzw. VMAF.
- Das Arbeitsverzeichnis liegt standardmäßig auf dem Ziel-Laufwerk
  (`<Ziel>/.framewise-work`), damit fertige Dateien nur umbenannt statt kopiert werden.
- Hardware-Encoding (NVENC, QSV, VAAPI, AMF, VideoToolbox) nach Funktionstest zur Laufzeit,
  mit CPU-Fallback.
- Queue mit Pause, Fortsetzen, Abbruch, Wiederholen, Prioritäten, Ressourcen-Gate und
  Wiederherstellung nach Neustart.
- Oberfläche auf Englisch und Deutsch.

## Voraussetzungen
- Rust ≥ 1.95 (Edition 2024; Vorgabe von egui/eframe 0.37)
- `ffmpeg` und `ffprobe` im `PATH` (oder Pfad in den Einstellungen); für HDR-Tonemapping
  wird ffmpeg mit `zscale` (zimg) benötigt, für VMAF mit `libvmaf`, für den HEVC-Vergleich
  in Profil E mit `libx265` (optional; ohne libx265 bleibt es bei H.264).
- Linux: X11- oder Wayland-Desktop

## Starten
```sh
cargo run --release
```
Die Sprache folgt der Systemsprache (`LANG`) und lässt sich in den Einstellungen umstellen.
Log-Ausgabe per `RUST_LOG=debug`.

## Entwicklung
```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test            # End-to-End-Tests werden übersprungen, wenn ffmpeg fehlt
```
Texte im Code sind englisch (`tr!("…")`); deutsche Übersetzungen stehen in
`locales/de.toml`. Ein Test schlägt fehl, wenn eine Übersetzung fehlt.

## Dokumentation
- [Architektur](docs/ARCHITECTURE.md)
- [Changelog](docs/CHANGELOG.md)
