using System.Collections.Generic;
using Autodesk.AutoCAD.ApplicationServices;

namespace AcLayerStandardizer.Core;

// Layer Herder's view of the window launcher. The process handling lives in the
// shared kit (UiLauncher); this file adds Layer Herder's snapshot and its
// first-run notice.
internal static class RustUiLauncher
{
    // Set once a launch has started the window in this AutoCAD process, so only the
    // first one passes the first-run notice.
    private static bool _noticeShown;

    // When the window from an earlier LSTDR is still running, brings it forward
    // and says so on the command line. Returns true when it did. Never throws.
    public static bool TryFocusExistingWindow(Document document) =>
        UiLauncher.TryFocusExisting(document, LayerHerderWindow.AlreadyOpenMessage);

    // A missing program file, AutoCAD not being ready, or a failed process start is
    // reported through the outcome (and detail), not thrown; the caller prints
    // LaunchGuard.DescribeFailure for it. IpcBridgeServer.Start and
    // SetDrawingSnapshot run outside that guard and can still throw, as before.
    public static LaunchOutcome TryLaunchFromActiveAutoCad(
        Document document,
        IEnumerable<string> sourceLayers,
        IEnumerable<string> emptyLayers,
        string templatePath,
        out string? detail)
    {
        var passNotice = false;
        return UiLauncher.TryLaunch(
            document,
            LayerHerderWindow.AlreadyOpenMessage,
            beforeLaunch: () =>
            {
                // Initialize may have attempted to start the pipe before AutoCAD was
                // ready. Recheck it in the command context before launching the client.
                IpcBridgeServer.Start();
                IpcBridgeServer.SetDrawingSnapshot(document, sourceLayers, emptyLayers, templatePath);
            },
            extraArguments: () =>
            {
                passNotice = NoticePolicy.ShouldPassNotice(_noticeShown);
                return passNotice ? " " + NoticePolicy.Argument : string.Empty;
            },
            onLaunched: () =>
            {
                if (passNotice) _noticeShown = true;
            },
            out detail);
    }
}
