# Changelog

Alle Änderungen, gegliedert nach den Phasen aus `ARCHITECTURE.md` (Abschnitt 7).
Nach jeder Phase liefen `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`
und `cargo test`.

## 0.1.0 – unveröffentlicht

### Phase 1 – Foundation
- Cargo-Projekt (Edition 2024, ein Crate mit `lib` + `bin`), Release-Profil mit Thin-LTO.
- `unsafe_code = "deny"`; einzige lokale Ausnahme: `encoding/os.rs` (nice, SIGSTOP/SIGCONT).
- Konfiguration als TOML (atomar gespeichert); eine defekte Datei wird nicht überschrieben,
  stattdessen starten Standardwerte.
- Logging per `tracing` auf stderr und in `<daten>/logs/framewise.log`; ein ffmpeg-Log pro Job.
- Erkennung von ffmpeg/ffprobe inkl. Version; Prozesse ausschließlich mit Argumentlisten
  (keine Shell), Timeout und `kill_on_drop`.
- Backend-Thread mit eigener Tokio-Runtime; die GUI kommuniziert nur über Kanäle und blockiert nie.
- Mehrsprachigkeit: Quelltexte englisch (`tr!`/`trf!`), deutsche Übersetzung in
  `locales/de.toml`. Ein Test prüft, dass jeder Text übersetzt ist und die Platzhalter passen.

### Phase 2 – Scanner und Analyse
- Domain-Modell für Container, Video, Audio und Untertitel, inklusive HDR10, HDR10+
  (Frame-Side-Data), HLG und Dolby Vision (Profil und Kompatibilität) sowie
  Atmos/DTS:X (bestätigt oder wahrscheinlich, mit Quelle der Erkennung).
- Rekursive Suche (MKV/MP4, Groß-/Kleinschreibung egal):
  - Symlink-Schleifen werden gemeldet.
  - Physische Duplikate werden über den kanonischen Pfad nur einmal gezählt.
  - Versteckte Dateien und ausgeschlossene Ordner werden übersprungen.
- Parallele ffprobe-Analyse über eine Semaphore.
- SQLite-Cache: Dateien mit unveränderter Größe und mtime werden nicht erneut analysiert.
- Empfehlungs-Engine: `Transcode`, `Remux`, `AlreadyOptimal` oder `Review`, mit
  Kompatibilität, Bit/Pixel, geschätzter Größe und Potenzial-Score. Einsparungen sind
  ausdrücklich als Schätzung gekennzeichnet.
- Top-N je Ordner und global.
- Duplikaterkennung: identischer Inhalt per Teil-Hash (erste und letzte 4 MiB) und gleicher Titel.
- Medienbibliothek: Filter, Sortierung, Mehrfachauswahl, Detailansicht mit allen Streams.

### Phase 3 – Encoding
- Profile A Universal, B High Quality, C Space Saver und D Custom, gespeichert in SQLite;
  die eingebauten Profile lassen sich auf den Standard zurücksetzen.
- `EncodePlan` als einzige Quelle für GUI-Warnungen, ffmpeg-Argumente, Validierung und
  Empfehlung.
- Zielauflösung: höchstens 1080p, nie hochskaliert, Seitenverhältnis (inkl. SAR und Rotation)
  bleibt erhalten, gerade Maße.
- Audio:
  - Strategien „Kompatibilität“ und „Original erhalten“.
  - AAC-Stereo je Sprache und genau eine Default-Spur.
  - Keine Spur entfällt ohne Eintrag im Plan.
- Untertitel:
  - Text wird zu `mov_text`.
  - Bitmap-Untertitel werden als Sidecar (`.sup`/`.mks`) exportiert, mit Warnung.
  - Erzwungene Untertitel erhalten zusätzlich ein `.forced.srt`, weil MP4 das
    forced-Flag nicht speichern kann.
  - Spurtitel werden zusätzlich als `handler_name` geschrieben, damit MP4 sie behält.
- Fortschritt über `-progress pipe:1`, mit Geschwindigkeit, fps und Restzeit.
- Ausgabesicherheit:
  - Die Ordnerstruktur der Quelle wird im Ziel nachgebildet.
  - Ziel darf nicht gleich der Quelle sein und nicht in ihr liegen.
  - Konfliktstrategien Überspringen, Umbenennen („Film (1).mp4“) und Ersetzen nur nach
    Bestätigung.
  - Gearbeitet wird im Arbeitsverzeichnis.
  - Ins Ziel kommt das Ergebnis über no-clobber-Platzierung (Hardlink bzw. Kopie mit
    `.partial`-Datei) – niemals durch Überschreiben.
- Validierung: Dauer (Toleranz max(1,5 s; 0,1 %)), Streams, Sprachen, Titel, Untertitel,
  Sidecars und optionaler Decode-Test. Fehlgeschlagene Ergebnisse kommen in die Quarantäne.

### Phase 4 – Hardware
- Laufzeit-Erkennung für NVENC, QSV, VAAPI, AMF und VideoToolbox: Encoderliste plus
  1-Frame-Funktionstest je Encoder. Hardware wird nie vorausgesetzt.
- GPU-Modelle per `nvidia-smi` und `lspci`, DRM-Render-Nodes.
- Hardware-Decoding lässt sich getrennt schalten; scheitert ein Hardware-Encode, folgt
  ein automatischer CPU-Fallback.

### Phase 5 – Queue und Ressourcen
- Scheduler mit Prioritäten und Parallelität; das Last-Gate prüft CPU-Zielwert, freien RAM
  und freien Platz im Arbeits- und Zielverzeichnis.
- nice-Level für ffmpeg; Pause und Fortsetzen eines Jobs über SIGSTOP/SIGCONT (Unix).
- Queue-Pause, Abbruch, Wiederholen, Entfernen, Priorität ändern.
- Persistenz der Jobs. Jobs, die beim Beenden liefen, werden zu `Interrupted`; die Queue
  startet dann pausiert. Verwaiste Arbeitsverzeichnisse werden entfernt.
- Hardware-Monitor: CPU (mit Verlauf), RAM, Swap, Datenträger und NVIDIA-GPU.
- Historie aller abgeschlossenen Jobs.

### Phase 6 – Qualität
- HDR → SDR per zscale und tonemap (hable/mobius/reinhard), BT.709-Tags; fehlt zscale,
  wird der Job blockiert statt stillschweigend falsch kodiert. Dolby Vision Profil 5
  wird als nicht korrekt tonemapbar markiert.
- Smart CRF: Binärsuche über Stichproben-Fenster nach dem höchsten CRF, der den
  Mindestwert (SSIM oder VMAF) erreicht. Ohne libvmaf wird SSIM verwendet, mit Hinweis.
- Space Saver erzwingt die Qualitätsprüfung.

### Phase 7 – Stabilisierung
- 68 Unit-Tests und 3 End-to-End-Tests mit echtem ffmpeg (werden übersprungen, wenn
  ffmpeg fehlt):
  - Legacy-Datei: MPEG-4, AC3 5.1, FLAC, forced SRT und ASS.
  - HDR10 → SDR inklusive Downscale.
  - Abbruch hinterlässt keine temporären Dateien.
- GUI-Laufzeittest mit Beispielbibliothek: Scan, Bibliothek, Vorschau, Queue (4 Jobs
  erfolgreich), Profile, Hardware, Historie, Einstellungen. Behobene Probleme:
  - fehlende Glyphen (eframe-Feature `system_fonts`; ⏶/⏷ durch ↑/↓ ersetzt),
  - Tabellen breiter als das Fenster (schmalere Spalten, horizontales Scrollen),
  - Zeilenklick auf Text wurde von auswählbaren Labels verschluckt,
  - Spaltenlinien der Queue überlagerten das Detailpanel,
  - Profil-Editor: mehrere Widgets pro Grid-Zeile landeten in Phantom-Spalten,
  - Profil-ID statt Profilname in Vorschau und Historie,
  - Ergebnis größer als die Quelle wird jetzt farblich hervorgehoben.

### Praxistest mit echter Blu-ray-Bibliothek (748 Dateien, 5,4 TB)
- **Scan:** 748 Dateien in etwa 40 s; die GUI blieb dabei bedienbar.
- **Encoding-Test** mit vier Release-Samples (DTS-HD MA, DTS-ES 6.1, HEVC, PGS, VobSub, *forced*):
  - Ergebnis: 3 erfolgreich, 1 zu Recht abgelehnt.
  - Das abgelehnte Sample ist defekt: Der Header nennt 60 s, die Datei enthält aber nur 4 s Daten.
- **Behoben – leere Untertitelspuren:** Quellspuren ohne einen einzigen Eintrag (häufig bei
  Samples und leeren Forced-Spuren) ließen den Job an der Sidecar-Prüfung scheitern.
  - Ein leeres Sidecar wird jetzt erkannt: keine Datei, 0 Bytes, unlesbar oder 0 Pakete.
  - Ist auch die Quellspur leer, wird die Datei verworfen und der Hinweis
    `SubtitleStreamEmpty` im Plan vermerkt.
  - Enthält die Quellspur Daten, schlägt die Validierung weiterhin fehl.
- **Behoben – Sidecar-Validierung:** Sie verlangt jetzt eine lesbare Datei mit mindestens einem
  Eintrag. Zuvor ging ein leeres `.mks` (nur Container-Header) durch.
- **Behoben – Thread-Begrenzung:** Das Limit „Encoder-Threads“ gilt jetzt auch für Decoder und
  Filter (`-threads` und `-filter_threads` vor `-i`). Vorher nutzte das Decodieren weiterhin
  alle Kerne.
- **GUI:**
  - Lange Fehlermeldungen in der Queue werden gekürzt (Tooltip); sie hatten die
    Aktionsspalte aus dem Bild geschoben.
  - Lange Dateinamen unter „Größte Dateien“ werden gekürzt.

### ffmpeg 8.1 mit VMAF
- Statischer Build ffmpeg 8.1 (FFmpeg-Builds/BtbN, SHA256 geprüft) mit `libvmaf`. Alle Tests
  laufen mit ffmpeg 4.4 und 8.1.
- **Behoben – Farbinformation nur auf Frame-Ebene:** ffprobe 8.1 meldet die Farbinformation
  mancher Dateien nur auf Frame-Ebene, auf Stream-Ebene steht „unknown“. Echtes HDR wäre dann
  als SDR eingestuft und nicht getonemappt worden. Fehlen bei 10-/12-Bit-Video Transfer oder
  Primaries, liest der Scanner sie jetzt aus dem ersten Frame.
- **libvmaf mit Thread-Begrenzung:** libvmaf rechnet mit `n_threads` gemäß „Encoder-Threads“
  (vorher auf einem Kern).
- **Hardware-Test:** Mit dem statischen Build schlägt VAAPI fehl, weil die System-libva zu alt
  ist („vaMapBuffer2“). Der 1-Frame-Funktionstest erkennt das, die CPU wird verwendet.

### Phase 8 – Erkenntnisse aus dem Praxistest
- **Behoben – Smart-CRF-Suchprotokoll ging verloren:** Die Schlussmessung des fertigen
  Ergebnisses ersetzte das Suchergebnis samt Liste der getesteten CRF-Werte. Das Protokoll
  bleibt jetzt erhalten. Queue-Details zeigen es („CRF-Suche: CRF 23: 95,1 → …“), die
  Historie zeigt Wert und CRF.
- **Größenschätzung aus echten Ergebnissen:**
  - Jeder erfolgreiche, validierte Re-Encode speichert in der Historie, wie weit die
    Schätzung des Videoanteils danebenlag. Audio und Overhead werden herausgerechnet.
  - Ab 2 Ergebnissen je Zielcodec korrigiert der Median dieser Abweichungen (der letzten 50
    Ergebnisse, Ausreißer außerhalb von 0,2×–5× verworfen) alle neuen Schätzungen.
  - Die Detailansicht und die Dashboard-Kachel nennen die Grundlage: kalibriert mit N Jobs
    oder Faustwert.
  - Es bleibt eine Schätzung.
- **Restzeit der Queue:**
  - Grundlage ist die aktuelle Encoding-Geschwindigkeit, ersatzweise frühere Jobs; die
    Anzahl paralleler Jobs ist berücksichtigt.
  - Nur eine Dauer, keine Uhrzeit: Die App rechnet bislang ohne Zeitzone (UTC).
- **Desktop-Benachrichtigungen** (abschaltbar unter Einstellungen → Oberfläche):
  - Gemeldet werden ein fertiger oder fehlgeschlagener Job und die abgeschlossene Queue.
  - Linux nutzt `notify-send`: Argumentliste ohne Shell, `--` vor dem Text, sodass
    Dateinamen keine Optionen werden können. Andere Plattformen bislang nicht.
- **Smart CRF schneller:**
  - Jeder Probeausschnitt wird einmal gelesen, gefiltert (Skalierung/Tonemapping) und
    verlustfrei (FFV1) im Arbeitsverzeichnis abgelegt.
  - Probe-Encodes und Messungen aller Suchschritte arbeiten mit diesen Referenzen. Bisher
    wurde die Quelle je Schritt und Ausschnitt zweimal gelesen und gefiltert.
  - Die Probe-Frames sind bitidentisch mit dem bisherigen Weg (verifiziert mit ffmpeg 4.4
    und 8.1). Gilt nur für CPU-Encoding; Hardware-Encoder nutzen weiter den alten Weg.
  - Die Suche startet beim Profil-CRF und interpoliert statt zu halbieren. Nach drei
    Schritten folgt Bisektion. Sie stoppt früh, wenn der nächste CRF die Schwelle laut
    Steigung sicher verfehlt (konservativ). Kein CRF wird doppelt gemessen.
  - Simuliert über 2.662 Qualitätskurven: im Mittel 2,7 statt 3,3 Schritte; in 1 Fall ein
    CRF konservativer als das Optimum.
- Neuer End-to-End-Test: Smart CRF mit HDR-Quelle (Referenzen, Suchprotokoll, Aufräumen).
- **Behoben – Schlussmessung verglich die falschen Frames:**
  - Die Qualitätsmessung des fertigen Ergebnisses ordnete die Frames von Quelle (MKV) und
    Ergebnis (MP4) über Zeitstempel einander zu.
  - MKV rundet Zeitstempel auf Millisekunden (0,042), MP4 speichert sie exakt (0,041708).
    Dadurch wurden benachbarte Frames verglichen.
  - Mit SSIM fiel das kaum auf, mit VMAF massiv. Gemessen an echten Ergebnissen aus dem
    Praxistest (VMAF vorher → nachher):

    | Sample | vorher | nachher |
    |---|---|---|
    | Baymax | 29,1 | 98,6 |
    | Guardians 2 | 89,5 | 97,1 |
    | Star Wars IV | 50,2 | 96,1 |

  - Space-Saver-Jobs (Pflichtprüfung) wären damit zu Unrecht gescheitert, und die
    Smart-CRF-Schlussprüfung hätte falsche Werte gezeigt.
  - Jetzt gilt:
    - Frames werden nach Nummer gepaart, mit gemeinsamer Zeitbasis
      (`settb=1/25,setpts=N`).
    - Messfenster beginnen in der Mitte zwischen zwei Frames.
    - Pro Fenster bestimmt ein kurzer Inhaltsvergleich (SSIM der ersten 12 Frames bei
      Versatz −1/0/+1) den tatsächlichen Versatz. ffmpeg 4.4 springt in MP4 und MKV
      gelegentlich um einen Frame verschieden.
  - Gegenprobe: Ein verlustfreies Ergebnis misst jetzt exakt SSIM 1,000; vorher 0,93–0,97.
  - Der Smart-CRF-End-to-End-Test vergleicht wieder Suche und Schlussmessung (Toleranz 0,01)
    und besteht mit ffmpeg 4.4 und 8.1.
  - Die CRF-Suche selbst war nicht betroffen: Probe und Referenz haben dort dasselbe
    Zeitraster.

### Phase 9 – Automatische Verfahrenswahl
Ausgangspunkt: Der Vergleich zweier echter Läufe desselben Films (True Justice, 16,4 GB).
- Lauf 1, feste CRF 20: 4,19 GB, VMAF 97,2 im Mittel und 96,2 an der schwächsten Stelle.
- Lauf 2, Smart CRF mit Ziel 93: CRF 25, 2,47 GB, VMAF 94,0 im Mittel und 93,1 an der
  schwächsten Stelle.

Die Probe-Encodes der Suche sagten das Ergebnis beider Läufe bis auf ±0,04 VMAF voraus.
Welches Verfahren das beste ist, hängt vom Film ab. Deshalb wird es jetzt pro Film gemessen,
statt fest im Profil zu stehen.

- **Neues eingebautes Profil „E · Automatisch“:** Vorgegeben wird das Ziel, nicht das
  Verfahren: VMAF ≥ 95 an der schwächsten Probe, Analysebudget 30 min. Die App wählt pro
  Film:
  1. **Repräsentative Proben:**
     - ffprobe liest die Paketgrößen der Videospur über den ganzen Film, nur demuxen, ohne
       Decodieren. Daraus entsteht eine Komplexitätskarte, Bytes pro Sekunde.
     - Gewählt werden 8 Proben à 10 s: die Hälfte an den aufwendigsten Stellen (mit
       Mindestabstand, damit nicht alle aus einer Actionszene stammen), der Rest gleichmäßig
       verteilt.
     - Intro und Abspann (je 5 %) bleiben außen vor.
     - Gibt es keine Karte (Fehler, zu kurzer Film), fällt die Auswahl auf gleichmäßige
       Proben zurück, mit Hinweis.
     - Die Schlussmessung nutzt dieselben Stellen.
  2. **Qualitätsgrenze an der schwächsten Probe:** neue Profiloption, gilt für Suche und
     Schlussmessung. Bisher zählte der Mittelwert.
  3. **H.264 gegen HEVC:**
     - H.264 wird immer gesucht und bleibt Standard.
     - Danach wird HEVC (x265 „medium“, Kennung `hvc1`) gesucht, sofern libx265 vorhanden
       ist, das Zeitbudget reicht und das Bild vergleichbar ist (gleiche Auflösung,
       HDR-Behandlung, Deinterlacing).
     - HEVC wird nur genommen, wenn es die Qualitätsgrenze hält und laut Proben mindestens
       15 % kleiner ist.
     - Schlägt die HEVC-Encodierung fehl, läuft der Job automatisch mit H.264 und der dafür
       gefundenen CRF weiter.
  4. **Lohnt sich nicht:** Spart das beste Verfahren unter 10 %, wird die Videospur
     kopiert, sofern sie MP4-tauglich ist. Gemessen wird gegen die Quellbytes derselben
     Stellen.
- Die Entscheidung wird im Job gespeichert und angezeigt (Queue-Details, Tooltip in der
  Historie):
  - gewähltes Verfahren
  - jeder Kandidat mit CRF, Mittel/schwächstem Wert, geschätzter Größe relativ zur Quelle
    und Suchdauer
  - Zeitpunkte der Proben und Hinweise (z. B. HEVC aus Zeitgründen nicht verglichen)
- Alle Optionen sind in jedem Profil einstellbar (Profile → Qualität): Probenauswahl,
  Grenze an schwächster Probe, HEVC-Vergleich mit Preset und Mindestersparnis,
  Kopierschwelle, Analysebudget. Die Standardwerte lassen bestehende Profile unverändert.
- **Behoben – x265 ignorierte die CPU-Drosselung:** x265 baut seinen eigenen Thread-Pool
  über alle Kerne und beachtet `-threads` nicht. HEVC-CPU-Encodes erhalten jetzt
  `-x265-params pools=<Threads>` und `log-level=error`.
- Neue Tests:
  - Komplexitätskarte und Probenauswahl
  - HEVC-Argumente
  - End-to-End-Test mit einer Rauschszene in einem 60-s-Clip: Die Szene wird als Probe
    gewählt, H.264 und HEVC werden verglichen, der Codec der Ausgabe passt zur
    Entscheidung, Suche und Schlussmessung stimmen überein. Läuft mit ffmpeg 4.4 und 8.1.
- Nicht enthalten: Erkennung dunkler Szenen. Sie bräuchte ein Decodieren des ganzen Films;
  Paketgrößen messen nur die Komplexität.
- **Arbeitsverzeichnis standardmäßig auf dem Ziel-Laufwerk:**
  - Ohne eigene Einstellung lag es bisher in den Anwendungsdaten
    (`~/.local/share/framewise/work`), also auf der Systemplatte. Zwischenergebnisse,
    Proben und FFV1-Referenzen können dort viele GB belegen.
  - Standard ist jetzt `<Zielverzeichnis>/.framewise-work`. Fertige Ergebnisse werden
    dadurch auf demselben Dateisystem umbenannt statt über Laufwerke kopiert.
  - Ein explizit gesetztes Arbeitsverzeichnis gilt weiter. Ohne Zielverzeichnis bleibt es
    bei den Anwendungsdaten.
- **Zeitbudget als harte Grenze:**
  - Im Praxistest (True Justice, VMAF ≥ 95) übersprang die erste Version den HEVC-Vergleich.
    Sie schätzte die HEVC-Suche pauschal als doppelt so lang wie die H.264-Suche
    (12,7 min), und das hätte das 30-Minuten-Budget überschritten.
  - Jetzt startet die HEVC-Suche, solange mindestens 2 Minuten Budget übrig sind. Ist das
    Budget erreicht, wird sie abgebrochen (ffmpeg beendet, Reste gehen mit dem
    Arbeitsverzeichnis). Es bleibt dann bei H.264, mit Hinweis in der Entscheidung.
  - Das Budget wird nie überschritten, und HEVC wird nicht mehr auf Verdacht ausgelassen.
- Profil-Editor: Die HEVC-Mindestersparnis hat eine eigene Zeile; vorher wurde „15 % kleiner“
  abgeschnitten.
- Ergebnis desselben Praxistests:
  - Bei CRF 20 erreichte die Szene bei 52:10 nur VMAF 93,7. Die drei gleichmäßig
    verteilten Stellen der früheren Läufe hatten sie nicht erfasst.
  - Die Suche wählte deshalb CRF 18, dort lag die schwächste Probe bei 95,2.
