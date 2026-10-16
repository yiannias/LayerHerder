using System.Collections.Generic;
using Autodesk.AutoCAD.ApplicationServices;
using Autodesk.AutoCAD.DatabaseServices;

namespace AcLayerStandardizer.Core;

// Connects Layer Herder's behavior to the shared AcadBridgeKit: the drawing
// fingerprint, the close guard, the log writer and the product name. Call
// Install once, before the kit's tracker or pipe server starts.
internal static class LayerHerderHost
{
    public static void Install()
    {
        BridgeServer.ProductName = "Layer Herder";
        BridgeServer.Logger = IpcBridgeServer.Log;
        UiLauncher.ExecutableName = LayerHerderWindow.ProgramFile;
        ActiveDrawingTracker.Fingerprint = database => LayerFingerprint.Compute(ReadLayerNames(database));
        ActiveDrawingTracker.BeginClose = OnBeginDocumentClose;
    }

    // Cancels the close when the mapping window reported unapplied connections
    // for this drawing within the last few seconds. Decides instantly from the
    // last report and never waits on the window or the pipe; any failure here
    // allows the close, because the warning must never trap the user.
    private static void OnBeginDocumentClose(Document document, DocumentBeginCloseEventArgs e)
    {
        var drawingId = ActiveDrawingTracker.GetDrawingId(document);
        var kind = CloseKind.Drawing;
        try { kind = CloseGuard.ClassifyClose(document.CommandInProgress); }
        catch (System.Exception) { /* unreadable: treat as a plain drawing close */ }
        var (pending, lastCheckIn) = PendingRegistry.Current;
        if (!CloseGuard.ShouldVeto(pending, lastCheckIn, DateTime.UtcNow, kind == CloseKind.Quit ? null : drawingId)) return;

        // The window must hear about the blocked close, or nothing would ever
        // ask the user: veto only once CloseBlocked is on the feed.
        var kindName = kind == CloseKind.Quit ? "quit" : "drawing";
        if (!CloseGuard.BlockClose(
                () => ActiveDrawingTracker.TryPublish("CloseBlocked", () => new { kind = kindName, drawing_id = drawingId }),
                e.Veto))
            IpcBridgeServer.Log("Close check: the window could not be told; allowing the close.");
    }

    private static List<string> ReadLayerNames(Database database)
    {
        var names = new List<string>();
        using var transaction = database.TransactionManager.StartOpenCloseTransaction();
        var layers = (LayerTable)transaction.GetObject(database.LayerTableId, OpenMode.ForRead);
        foreach (ObjectId id in layers)
            names.Add(((LayerTableRecord)transaction.GetObject(id, OpenMode.ForRead)).Name);
        return names;
    }
}
