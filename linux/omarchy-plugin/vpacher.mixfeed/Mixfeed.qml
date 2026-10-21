// Mixfeed bar widget: state of mixfeedd's "Mixfeed Mic", and a keyboard
// panel to choose the NDI source and the reference level. Talks to mixfeedd only
// through mfctl (~/.local/bin, installed by linux/install.sh).
//
// Panel keys: j/k or arrows move, Enter/Space select, r refresh, Esc close,
// Tab switches to the next bar panel.
pragma ComponentBehavior: Bound

import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

Panel {
  id: root
  moduleName: "vpacher.mixfeed"
  ipcTarget: "vpacher.mixfeed"

  readonly property string mfctl: Quickshell.env("HOME") + "/.local/bin/mfctl"
  readonly property var levels: [
    { key: "smpte", title: "NDI SMPTE", tip: "Most NDI senders (NDI Tools, NDI devices): +4 dBu = -20 dBFS, ×0.1" },
    { key: "ebu", title: "NDI EBU", tip: "Senders aligned to EBU R68, for example BirdDog converters: +4 dBu = -14 dBFS, ×0.2" },
    { key: "raw", title: "Raw 0 dB", tip: "No change (×1), for senders that send full scale as 1.0, for example OBS with the DistroAV plug-in" }
  ]
  property var status: null
  property var rxSettings: null
  property var sources: []
  property bool daemonUp: false
  // Keyboard cursor over all choices: the source rows ("None" last), then the levels.
  property bool cursorActive: false
  property int cursorIndex: 0

  readonly property color fg: bar ? bar.foreground : Color.foreground
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family
  readonly property string rxState: status ? status.state : ""
  readonly property bool receiving: rxState === "connected" && status.io
  readonly property var camera: status && status.camera ? status.camera : null
  readonly property bool cameraLive: !!camera && camera.live
  // mixfeedd counts a muted microphone as not on air.
  readonly property bool micOnAir: !!status && status.on_air
  readonly property bool muted: !!status && status.muted
  readonly property bool monitoring: !!status && !!status.monitor
  // Tally: lit while an application (not the monitor) takes the microphone, or the camera.
  readonly property bool onAir: micOnAir || cameraLive
  readonly property bool broken: !daemonUp || rxState === "error" || rxState === "lost"
  readonly property var sourceRows: sources.concat([""])
  // Choices for the cursor: sources, levels, then "keep connected" and "monitor".
  readonly property int choiceCount: sourceRows.length + levels.length + 3
  // The setting is a name part; mixfeedd connects to the first match.
  readonly property string matchedSource: {
    if (status && status.source) return status.source
    var part = rxSettings ? rxSettings.source : ""
    if (!part) return ""
    for (var i = 0; i < sources.length; i++) if (sources[i].indexOf(part) >= 0) return sources[i]
    return ""
  }

  // Engine text without its "error: " / "lost: " prefix.
  function detailText() {
    return status ? status.text.replace(/^(error|lost): /, "") : ""
  }

  // The NDI receiver reconnects by itself; "lost: no audio from <source>"
  // means the sender stopped sending (quit, crash, network).
  function stoppedSending() {
    return rxState === "lost" && status.text.indexOf("lost: no audio from ") === 0
  }

  function stateText() {
    if (!daemonUp) return "mixfeedd not running"
    switch (rxState) {
    case "connected": return status.io ? "Receiving" : (rxSettings && rxSettings.keep_warm ? "Ready" : "Standby")
    case "searching": return "Searching"
    case "no-source": return "No source selected"
    case "idle": return "Idle"
    case "lost": return stoppedSending() ? "Source stopped sending" : "Connection lost"
    case "error": return detailText()
    }
    return rxState
  }

  function tooltipText() {
    var t = tooltipBase()
    return daemonUp && muted ? t + " · microphone muted" : t
  }

  function tooltipBase() {
    if (!daemonUp) return "Mixfeed: mixfeedd not running (install: github.com/vpacher/mixfeed)"
    if (onAir) return "Mixfeed: live (" + [micOnAir ? "microphone" : "", cameraLive ? "camera" : ""].filter(function(s) { return s }).join(", ") + ")"
      + (cameraLive && camera.source_width ? " · camera source " + cameraSourceDetail() : "")
    if (rxState === "connected") return "Mixfeed: " + stateText() + " " + status.source
    if (rxState === "searching") return "Mixfeed: searching for “" + (rxSettings ? rxSettings.source : "") + "”"
    if (stoppedSending()) return "Mixfeed: " + detailText().replace("no audio from ", "") + " stopped sending"
    if (rxState === "error") return "Mixfeed: " + detailText()
    return "Mixfeed: " + stateText()
  }

  // State of the video receiver, from its status line: state=<state> source="…" …
  function cameraState() {
    var m = camera ? /^state=(.*?) source=/.exec(camera.text) : null
    return m ? m[1] : ""
  }

  function cameraText() {
    if (!camera) return ""
    if (camera.error) return "Off: " + camera.error
    var st = cameraState()
    if (st.indexOf("error: ") === 0) return "Error: " + st.slice(7)
    if (!camera.live) return "Ready (not in use)"
    if (st.indexOf("lost: no video") === 0) return "Live · source stopped sending"
    if (st.indexOf("searching ") === 0) return "Live · searching for the source"
    if (!camera.source_width) return "Live"
    var rate = frameRate(camera.source_rate_n, camera.source_rate_d)
    return "Live · " + formatName(camera.source_width, camera.source_height) + (rate ? " · " + rate + " fps" : "")
  }

  // Common name of a picture size: 4K, 1080p, 720p …; other sizes as W×H.
  // The receiver asks NDI for progressive frames only.
  function formatName(w, h) {
    if (h === 2160 && (w === 3840 || w === 4096)) return "4K"
    if (h === 4320 && w === 7680) return "8K"
    if (w === Math.round(h * 16 / 9) || (w === 720 && (h === 576 || h === 480))) return h + "p"
    return w + "×" + h
  }

  // Frames per second: whole numbers plain, NTSC rates with two decimals (59.94).
  function frameRate(n, d) {
    if (!n || !d) return ""
    var v = n / d
    return Math.abs(v - Math.round(v)) < 0.005 ? String(Math.round(v)) : v.toFixed(2)
  }

  // Full detail for the tooltip: 3840×2160 UYVY, 25 fps.
  function cameraSourceDetail() {
    var rate = frameRate(camera.source_rate_n, camera.source_rate_d)
    return camera.source_width + "×" + camera.source_height + " " + camera.source_fourcc + (rate ? ", " + rate + " fps" : "")
  }

  // Peak of the audio the applications get (after the reference level).
  function micText() {
    if (muted) return status.io ? "Muted (an application listens, gets silence)" : "Muted"
    var p = status ? status.peak_db : null
    var level = p === null || p === undefined ? "no audio"
      : p <= -90 ? "silent"
      : "peak " + p.toFixed(1) + " dB" + (p >= -0.1 ? " (clipping)" : "")
    return (micOnAir ? "Live" : "Monitor") + " · " + level
  }

  function num(v, digits, unit, signed) {
    if (v === null || v === undefined) return "—"
    return (signed && v >= 0 ? "+" : "") + v.toFixed(digits) + unit
  }

  function refresh() {
    if (poll.running) return
    // With --sources the request also keeps mixfeedd browsing, so only while the panel is open.
    poll.command = [mfctl, "json"].concat(opened ? ["--sources"] : [])
    poll.running = true
  }

  function run(args) {
    Quickshell.execDetached([mfctl].concat(args))
    soon.restart()
  }

  function selectSource(name) { run(["source", name]) }
  function selectLevel(key) { run(["level", key]) }
  function toggleMute() { run(["mute", muted ? "off" : "on"]) }
  function toggleKeepWarm() { run(["warm", rxSettings && rxSettings.keep_warm ? "off" : "on"]) }
  function toggleMonitor() { run(["monitor", monitoring ? "off" : "on"]) }

  function moveCursor(delta) {
    if (!cursorActive) {
      cursorActive = true
      return
    }
    cursorIndex = Math.max(0, Math.min(choiceCount - 1, cursorIndex + delta))
  }

  function activateCursor() {
    if (!cursorActive) return
    if (cursorIndex < sourceRows.length) selectSource(sourceRows[cursorIndex])
    else if (cursorIndex < sourceRows.length + levels.length) selectLevel(levels[cursorIndex - sourceRows.length].key)
    else if (cursorIndex === sourceRows.length + levels.length) toggleMute()
    else if (cursorIndex === sourceRows.length + levels.length + 1) toggleKeepWarm()
    else toggleMonitor()
  }

  onOpenedChanged: {
    if (opened) {
      cursorActive = false
      var i = sourceRows.indexOf(matchedSource)
      cursorIndex = i >= 0 ? i : 0
      refresh()
    }
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  Timer {
    interval: root.opened ? 1000 : 2000
    running: true
    repeat: true
    triggeredOnStart: true
    onTriggered: root.refresh()
  }

  Timer {
    id: soon
    interval: 300
    onTriggered: root.refresh()
  }

  Process {
    id: poll
    stdout: StdioCollector {
      id: out
      waitForEnd: true
      onStreamFinished: {
        try {
          var v = JSON.parse(out.text)
          root.status = v.status
          root.rxSettings = v.settings
          if (v.sources) root.sources = v.sources
          root.daemonUp = true
        } catch (e) {
          root.daemonUp = false
        }
      }
    }
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    // Webcam like the macOS menu bar icon (microphone and camera are one device for
    // the user); webcam-off when the service or the source is gone.
    text: root.broken ? "󱜷" : "󰖠"
    active: root.onAir
    tooltipText: root.tooltipText()
    onPressed: root.toggle()
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(380))
    contentHeight: panel.fittedContentHeight(column.implicitHeight)

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onMoveRequested: function(dx, dy) { root.moveCursor(dy !== 0 ? dy : dx) }
      onActivateRequested: root.activateCursor()
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) { if (t === "r") root.refresh() }

      Column {
        id: column
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        spacing: Style.space(14)

        // ---------- Hero: webcam icon · title/state ----------
        Item {
          width: parent.width
          implicitHeight: Math.max(heroIcon.implicitHeight, heroLabels.implicitHeight)

          Text {
            id: heroIcon
            textFormat: Text.PlainText
            text: root.broken ? "󱜷" : "󰖠"
            color: root.fg
            font.family: root.fontFamily
            font.pixelSize: Style.font.display
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
          }

          Column {
            id: heroLabels
            anchors.left: heroIcon.right
            anchors.leftMargin: Style.space(14)
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(2)

            Text {
              textFormat: Text.PlainText
              text: "Mixfeed Mic"
              color: root.fg
              font.family: root.fontFamily
              font.pixelSize: Style.font.title
              font.bold: true
              elide: Text.ElideRight
              width: parent.width
            }

            Text {
              textFormat: Text.PlainText
              text: (root.stateText() + (root.rxState === "connected" ? " · " + root.status.source : "")).toUpperCase()
              color: Qt.darker(root.fg, 1.4)
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              font.bold: true
              font.letterSpacing: 1.2
              elide: Text.ElideRight
              width: parent.width
            }
          }
        }

        // ---------- Without the daemon (for example installed from the marketplace only) ----------
        Column {
          visible: !root.daemonUp
          width: parent.width
          spacing: Style.space(4)

          InfoLabel {
            width: parent.width
            wrapMode: Text.WordWrap
            text: "The widget needs the mixfeedd service. If it is not installed, install Mixfeed with linux/install.sh from:"
          }

          InfoValue {
            text: "github.com/vpacher/mixfeed"
            font.underline: installLink.containsMouse

            MouseArea {
              id: installLink
              anchors.fill: parent
              hoverEnabled: true
              cursorShape: Qt.PointingHandCursor
              onClicked: Qt.openUrlExternally("https://github.com/vpacher/mixfeed")
            }
          }
        }

        // ---------- Microphone level (while a client takes audio) ----------
        InfoPair {
          visible: !!root.status && (root.status.io || root.status.muted)
          label: "Microphone"
          value: root.micText()
        }

        // ---------- Camera ----------
        InfoPair {
          visible: !!root.camera
          label: "Camera"
          value: root.cameraText()
        }

        // ---------- Receive figures (while a client takes audio) ----------
        Row {
          visible: !!root.status && root.status.io
          width: parent.width
          spacing: Style.space(20)

          Column {
            width: (parent.width - parent.spacing) / 2
            spacing: Style.spacing.labelGap
            InfoPair { label: "Buffer"; value: root.num(root.status ? root.status.fill_ms : null, 0, " ms", false) }
            InfoPair { label: "Sender clock"; value: root.num(root.status ? root.status.sender_ppm : null, 1, " ppm", true) }
          }

          Column {
            width: (parent.width - parent.spacing) / 2
            spacing: Style.spacing.labelGap
            InfoPair { label: "Underruns"; value: root.status ? String(root.status.underruns) : "—" }
            InfoPair { label: "Resyncs"; value: root.status ? String(root.status.resyncs) : "—" }
          }
        }

        // ---------- Source picker ----------
        PanelSeparator {
          foreground: root.fg
        }

        Column {
          width: parent.width
          spacing: Style.space(6)

          // The NDI SDK licence asks for a link to ndi.video where a source is chosen.
          Item {
            width: parent.width
            height: sourceHeader.implicitHeight

            PanelSectionHeader {
              id: sourceHeader
              text: "NDI® SOURCE"
              foreground: root.fg
              fontFamily: root.fontFamily
            }

            InfoLabel {
              anchors.right: parent.right
              anchors.baseline: sourceHeader.baseline
              text: "ndi.video"
              font.underline: ndiLink.containsMouse

              MouseArea {
                id: ndiLink
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onClicked: Qt.openUrlExternally("https://ndi.video/")
              }
            }
          }

          Repeater {
            model: root.sourceRows
            Button {
              required property string modelData
              required property int index
              width: parent.width
              text: modelData === "" ? "None" : modelData
              fontSize: Style.font.bodySmall
              foreground: root.fg
              fontFamily: root.fontFamily
              horizontalPadding: Style.spacing.controlPaddingX
              verticalPadding: Style.spacing.controlPaddingY
              active: modelData === ""
                ? (!!root.rxSettings && root.rxSettings.source === "")
                : modelData === root.matchedSource
              hasCursor: root.cursorActive && root.cursorIndex === index
              onClicked: root.selectSource(modelData)
              onHovered: function(h) {
                if (h) {
                  root.cursorActive = true
                  root.cursorIndex = index
                }
              }
            }
          }

          Text {
            visible: root.sources.length === 0
            textFormat: Text.PlainText
            text: root.daemonUp ? "Searching the network…" : "Start mixfeedd: systemctl --user start mixfeedd"
            color: Qt.darker(root.fg, 1.4)
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
          }
        }

        // ---------- Reference level ----------
        PanelSeparator {
          foreground: root.fg
        }

        Column {
          width: parent.width
          spacing: Style.space(10)

          PanelSectionHeader {
            text: "REFERENCE LEVEL"
            foreground: root.fg
            fontFamily: root.fontFamily
          }

          Row {
            id: levelRow
            width: parent.width
            spacing: Style.space(6)

            readonly property real cellWidth: (width - spacing * (root.levels.length - 1)) / root.levels.length

            Repeater {
              model: root.levels
              Button {
                required property var modelData
                required property int index
                width: levelRow.cellWidth
                text: modelData.title
                tooltipText: modelData.tip
                fontSize: Style.font.bodySmall
                foreground: root.fg
                fontFamily: root.fontFamily
                horizontalPadding: Style.spacing.controlPaddingX
                verticalPadding: Style.spacing.controlPaddingY + Style.space(2)
                bordered: true
                active: !!root.rxSettings && root.rxSettings.level === modelData.key
                hasCursor: root.cursorActive && root.cursorIndex === root.sourceRows.length + index
                onClicked: root.selectLevel(modelData.key)
                onHovered: function(h) {
                  if (h) {
                    root.cursorActive = true
                    root.cursorIndex = root.sourceRows.length + index
                  }
                }
              }
            }
          }
        }

        // ---------- Keep ready ----------
        PanelSeparator {
          foreground: root.fg
        }

        Column {
          width: parent.width
          spacing: Style.space(10)

          PanelSectionHeader {
            text: "MICROPHONE"
            foreground: root.fg
            fontFamily: root.fontFamily
          }

          Button {
            readonly property int choice: root.sourceRows.length + root.levels.length
            width: parent.width
            text: root.muted ? "Muted" : "Mute"
            tooltipText: "Send silence and stay connected: applications keep the microphone, unmuting is instant. A muted microphone is not on air."
            fontSize: Style.font.bodySmall
            foreground: root.fg
            fontFamily: root.fontFamily
            horizontalPadding: Style.spacing.controlPaddingX
            verticalPadding: Style.spacing.controlPaddingY + Style.space(2)
            bordered: true
            active: root.muted
            hasCursor: root.cursorActive && root.cursorIndex === choice
            onClicked: root.toggleMute()
            onHovered: function(h) {
              if (h) {
                root.cursorActive = true
                root.cursorIndex = choice
              }
            }
          }

          Button {
            readonly property int choice: root.sourceRows.length + root.levels.length + 1
            width: parent.width
            text: "Keep connected"
            tooltipText: "Stay connected to the source without an application: the microphone is ready at once. The camera always connects on demand."
            fontSize: Style.font.bodySmall
            foreground: root.fg
            fontFamily: root.fontFamily
            horizontalPadding: Style.spacing.controlPaddingX
            verticalPadding: Style.spacing.controlPaddingY + Style.space(2)
            bordered: true
            active: !!root.rxSettings && root.rxSettings.keep_warm
            hasCursor: root.cursorActive && root.cursorIndex === choice
            onClicked: root.toggleKeepWarm()
            onHovered: function(h) {
              if (h) {
                root.cursorActive = true
                root.cursorIndex = choice
              }
            }
          }

          Button {
            readonly property int choice: root.sourceRows.length + root.levels.length + 2
            width: parent.width
            text: root.monitoring ? "Monitor on (" + root.status.monitor + ")" : "Monitor"
            tooltipText: "Play the microphone to the default output (headphones). The monitor does not count as on air."
            fontSize: Style.font.bodySmall
            foreground: root.fg
            fontFamily: root.fontFamily
            horizontalPadding: Style.spacing.controlPaddingX
            verticalPadding: Style.spacing.controlPaddingY + Style.space(2)
            bordered: true
            active: root.monitoring
            hasCursor: root.cursorActive && root.cursorIndex === choice
            onClicked: root.toggleMonitor()
            onHovered: function(h) {
              if (h) {
                root.cursorActive = true
                root.cursorIndex = choice
              }
            }
          }

          InfoLabel {
            width: parent.width
            wrapMode: Text.WordWrap
            opacity: 0.45
            text: "NDI® is a registered trademark of Vizrt NDI AB."
          }
        }
      }
    }
  }

  component InfoPair: Row {
    id: pair
    property string label: ""
    property string value: ""

    width: parent.width
    spacing: Style.space(8)

    InfoLabel { id: pairLabel; text: pair.label }
    Item { width: Math.max(0, pair.width - pairLabel.implicitWidth - pairValue.implicitWidth - pair.spacing * 2); height: 1 }
    InfoValue { id: pairValue; text: pair.value }
  }

  component InfoLabel: Text {
    textFormat: Text.PlainText
    color: root.fg
    opacity: 0.6
    font.family: root.fontFamily
    font.pixelSize: Style.font.bodySmall
  }

  component InfoValue: Text {
    textFormat: Text.PlainText
    color: root.fg
    font.family: root.fontFamily
    font.pixelSize: Style.font.bodySmall
  }
}
