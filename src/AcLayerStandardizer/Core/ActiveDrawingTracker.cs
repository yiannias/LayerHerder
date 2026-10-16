using System.IO;
using System.Runtime.CompilerServices;
using Autodesk.AutoCAD.ApplicationServices;
using Autodesk.AutoCAD.DatabaseServices;

namespace AcLayerStandardizer.Core;

// Runs on AutoCAD's thread: watches document events and publishes the active
// drawing's identity and layer fingerprint into ActiveDrawingRegistry, so the
// pipe thread never has to call AutoCAD APIs.
public static class ActiveDrawingTracker
{
    private static readonly ConditionalWeakTable<Document, string> Ids = new();
    private static readonly RefreshGate Gate = new();
    private static bool _started;

    // Unique per open document for this AutoCAD session (file names can collide).
    public static string GetDrawingId(Document document) =>
        Ids.GetValue(document, _ => ActiveDrawingIds.Next());

    // Must run on AutoCAD's thread (inside a command context).
    public static Document? TryFindDocument(string drawingId)
    {
        foreach (Document document in Application.DocumentManager)
            if (GetDrawingId(document) == drawingId) return document;
        return null;
    }

    public static void Start()
    {
        if (_started) return;
        _started = true;

        var documents = Application.DocumentManager;
        documents.DocumentActivated += OnDocumentActivated;
        documents.DocumentCreated += OnDocumentCreated;
        documents.DocumentToBeDestroyed += OnDocumentToBeDestroyed;
        Application.Idle += OnIdle;
        foreach (Document document in documents)
            Watch(document);
        Refresh(documents.MdiActiveDocument);
    }

    public static void Stop()
    {
        if (!_started) return;
        _started = false;

        var documents = Application.DocumentManager;
        documents.DocumentActivated -= OnDocumentActivated;
        documents.DocumentCreated -= OnDocumentCreated;
        documents.DocumentToBeDestroyed -= OnDocumentToBeDestroyed;
        Application.Idle -= OnIdle;
        foreach (Document document in documents)
            Unwatch(document);
        ActiveDrawingRegistry.Clear();
    }

    private static void OnDocumentActivated(object? sender, DocumentCollectionEventArgs e)
    {
        var document = e.Document;
        if (document is not null)
            TryPublish("DrawingActivated", () => new
            {
                drawing_id = GetDrawingId(document),
                display_name = Path.GetFileName(document.Name)
            });
        Refresh(document);
    }

    // Feed publishing must never throw into an AutoCAD event handler or stop the
    // work that follows it. Returns whether the event was published.
    private static bool TryPublish(string type, Func<object> payload)
    {
        try
        {
            EventFeed.Shared.Publish(type, payload());
            return true;
        }
        catch (System.Exception ex)
        {
            IpcBridgeServer.Log($"Could not publish {type}: {ex.GetType().Name}: {ex.Message}");
            return false;
        }
    }

    private static void OnDocumentCreated(object? sender, DocumentCollectionEventArgs e) => Watch(e.Document);

    private static void OnDocumentToBeDestroyed(object? sender, DocumentCollectionEventArgs e)
    {
        try { Unwatch(e.Document); }
        catch (System.Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"Layer Herder could not unwatch the closing drawing: {ex.Message}");
        }

        string? closedId = null;
        try { closedId = GetDrawingId(e.Document); }
        catch (System.Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"Layer Herder could not identify the closing drawing: {ex.Message}");
        }

        if (closedId is not null)
            TryPublish("DrawingClosed", () => new { drawing_id = closedId });

        // If the id is unknown, clear unconditionally: a stale registry for a
        // closed drawing is worse than a redundant clear.
        var current = ActiveDrawingRegistry.Current;
        if (closedId is null || (current is not null && current.DrawingId == closedId))
            ActiveDrawingRegistry.Clear();
    }

    // Layers can change without a command ending (the Layer Properties palette,
    // this plugin's own Apply and Purge, LISP/ActiveX), so layer-table-record
    // events also mark the state dirty; the refresh itself waits for idle time.
    private static void Watch(Document document)
    {
        document.BeginDocumentClose += OnBeginDocumentClose;
        document.CommandEnded += OnCommandEnded;
        document.Database.ObjectAppended += OnLayerObjectChanged;
        document.Database.ObjectModified += OnLayerObjectChanged;
        document.Database.ObjectErased += OnLayerObjectErased;
    }

    private static void Unwatch(Document document)
    {
        document.BeginDocumentClose -= OnBeginDocumentClose;
        document.CommandEnded -= OnCommandEnded;
        document.Database.ObjectAppended -= OnLayerObjectChanged;
        document.Database.ObjectModified -= OnLayerObjectChanged;
        document.Database.ObjectErased -= OnLayerObjectErased;
    }

    // Cancels the close when the mapping window reported unapplied connections
    // for this drawing within the last few seconds. Decides instantly from the
    // last report and never waits on the window or the pipe; any failure here
    // allows the close, because the warning must never trap the user.
    private static void OnBeginDocumentClose(object? sender, DocumentBeginCloseEventArgs e)
    {
        try
        {
            if (sender is not Document document) return;
            var drawingId = GetDrawingId(document);
            var kind = CloseKind.Drawing;
            try { kind = CloseGuard.ClassifyClose(document.CommandInProgress); }
            catch (System.Exception) { /* unreadable: treat as a plain drawing close */ }
            var (pending, lastCheckIn) = PendingRegistry.Current;
            if (!CloseGuard.ShouldVeto(pending, lastCheckIn, DateTime.UtcNow, kind == CloseKind.Quit ? null : drawingId)) return;

            // The window must hear about the blocked close, or nothing would ever
            // ask the user: veto only once CloseBlocked is on the feed.
            var kindName = kind == CloseKind.Quit ? "quit" : "drawing";
            if (!CloseGuard.BlockClose(
                    () => TryPublish("CloseBlocked", () => new { kind = kindName, drawing_id = drawingId }),
                    e.Veto))
                IpcBridgeServer.Log("Close check: the window could not be told; allowing the close.");
        }
        catch (System.Exception ex)
        {
            IpcBridgeServer.Log($"Close check failed; allowing the close: {ex.GetType().Name}: {ex.Message}");
        }
    }

    private static void OnCommandEnded(object? sender, CommandEventArgs e) => Gate.MarkDirty();

    private static void OnLayerObjectChanged(object? sender, ObjectEventArgs e)
    {
        if (e.DBObject is LayerTableRecord) Gate.MarkDirty();
    }

    private static void OnLayerObjectErased(object? sender, ObjectErasedEventArgs e)
    {
        if (e.DBObject is LayerTableRecord) Gate.MarkDirty();
    }

    private static void OnIdle(object? sender, EventArgs e)
    {
        if (Gate.TryConsume()) Refresh(Application.DocumentManager.MdiActiveDocument);
    }

    private static void Refresh(Document? document)
    {
        try
        {
            if (document is null)
            {
                ActiveDrawingRegistry.Clear();
                return;
            }

            var before = ActiveDrawingRegistry.Current;
            var state = ActiveDrawingRegistry.Publish(
                GetDrawingId(document),
                Path.GetFileName(document.Name),
                LayerFingerprint.Compute(ReadLayerNames(document.Database)));
            // Publish returns the same state (same revision) when nothing changed,
            // so idle refreshes stay silent on the feed.
            if (before is null || before.Revision != state.Revision)
                TryPublish("LayersChanged", () => new
                {
                    drawing_id = state.DrawingId,
                    fingerprint = state.Fingerprint
                });
        }
        catch (System.Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"Layer Herder could not refresh the active drawing: {ex.Message}");
        }
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
