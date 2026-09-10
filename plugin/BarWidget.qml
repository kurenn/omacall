import QtQuick
import Quickshell
import Quickshell.Io

// Call state at a glance. Self-contained on purpose: the daemon already owns
// every piece of state worth having, so the widget just asks it and renders the
// answer rather than keeping a second copy in a shared singleton.
Item {
    id: widget

    property string moduleName: "io.github.kurenn.omacall"
    property bool opened: false
    property bool popoutSwitchClosing: false

    property string callState: "unknown"
    property int peerCount: 0
    property bool relayed: false
    property bool daemonRunning: false

    readonly property bool inCall: callState === "in_call"
    readonly property bool ringing: callState === "ringing_in" || callState === "ringing_out"

    implicitWidth: row.implicitWidth
    implicitHeight: row.implicitHeight

    function open() { opened = true }
    function close() { opened = false }
    function toggle() { opened ? close() : open() }
    function closeForPopoutSwitch() { popoutSwitchClosing = true; close() }

    // `omacall status` is the same JSON the CLI and the smoke test read, so
    // there is one description of a call rather than three.
    Process {
        id: poll
        command: ["omacall", "status"]
        stdout: StdioCollector {
            onStreamFinished: {
                try {
                    const s = JSON.parse(this.text)
                    widget.callState = s.state
                    widget.peerCount = s.peers.length
                    widget.relayed = s.paths_relay > 0 && s.paths_direct === 0
                    widget.daemonRunning = true
                } catch (e) {
                    widget.daemonRunning = false
                    widget.callState = "unknown"
                }
            }
        }
    }

    Timer {
        // A call changes state on human timescales; idle polling stays cheap.
        interval: widget.inCall || widget.ringing ? 1000 : 4000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: poll.running = true
    }

    Row {
        id: row
        spacing: 6
        anchors.centerIn: parent

        Text {
            anchors.verticalCenter: parent.verticalCenter
            text: widget.ringing ? "☎" : "●"
            color: widget.inCall
                   ? (widget.relayed ? "#DA9C4B" : "#58C4D6")
                   : "#8494A2"
            opacity: widget.daemonRunning ? 1.0 : 0.4

            SequentialAnimation on opacity {
                running: widget.ringing
                loops: Animation.Infinite
                NumberAnimation { to: 0.3; duration: 500 }
                NumberAnimation { to: 1.0; duration: 500 }
            }
        }

        Text {
            anchors.verticalCenter: parent.verticalCenter
            visible: widget.inCall
            // Saying "relay" out loud matters: a relayed call adds real
            // latency, and unlabelled it just reads as "omacall is laggy".
            text: widget.relayed ? widget.peerCount + " · relay" : "" + widget.peerCount
            color: "#B4C0CB"
        }
    }

    MouseArea {
        anchors.fill: parent
        acceptedButtons: Qt.LeftButton | Qt.RightButton
        onClicked: (mouse) => {
            if (!widget.daemonRunning) {
                // A manifest cannot declare dependencies, so offering the
                // install is the only way to point at the missing daemon.
                Quickshell.execDetached([
                    "omarchy-launch-floating-terminal-with-presentation",
                    "sudo pacman -S omacall && systemctl --user enable --now omacall.service"
                ])
            } else if (mouse.button === Qt.RightButton && widget.inCall) {
                Quickshell.execDetached(["omacall", "hangup"])
            } else {
                Quickshell.execDetached([
                    "omarchy-launch-floating-terminal-with-presentation", "omacall"
                ])
            }
        }
    }
}
