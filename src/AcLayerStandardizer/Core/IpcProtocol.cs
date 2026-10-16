using System.Text.Json;

namespace AcLayerStandardizer.Core;

// Pure protocol rules shared by the pipe server; no AutoCAD dependency.
public static class IpcProtocol
{
    public const int CurrentVersion = 4;
    public const int MinSupportedVersion = 2;

    public static bool IsSupportedVersion(int version) =>
        version >= MinSupportedVersion && version <= CurrentVersion;

    // Returns an error message when the request targets a different drawing
    // than the snapshot, or null when it is acceptable. A drawing id wins when
    // the client sent one; older clients fall back to the display name.
    public static string? CheckDrawingTarget(
        string snapshotDrawingId, string snapshotDrawingName, string? requestDrawingId, string requestDrawingName)
    {
        var matches = string.IsNullOrEmpty(requestDrawingId)
            ? string.Equals(requestDrawingName, snapshotDrawingName, StringComparison.OrdinalIgnoreCase)
            : string.Equals(requestDrawingId, snapshotDrawingId, StringComparison.Ordinal);
        return matches
            ? null
            : "AutoCAD is working on a different drawing than the Layer Herder window. "
                + SwitchAndTryAgain(requestDrawingName);
    }

    // The way out of a refusal while the mapping window is open: switch AutoCAD to
    // the drawing the window acts on. Never suggests running LSTDR again (that
    // would only bring the same window forward).
    public static string SwitchAndTryAgain(string? drawingName) =>
        string.IsNullOrWhiteSpace(drawingName)
            ? "Switch to the drawing shown in the Layer Herder window in AutoCAD and try again."
            : $"Switch to {drawingName} in AutoCAD and try again.";

    // Reads the window's "pending" report from a request payload. Never throws:
    // a missing, null or non-array report is empty, and entries without a
    // non-empty string drawing_id or a non-negative integer count are skipped.
    public static IReadOnlyList<PendingDrawing> ParsePending(JsonElement payload)
    {
        var result = new List<PendingDrawing>();
        try
        {
            if (payload.ValueKind != JsonValueKind.Object
                || !payload.TryGetProperty("pending", out var pending)
                || pending.ValueKind != JsonValueKind.Array)
                return result;

            foreach (var entry in pending.EnumerateArray())
            {
                if (entry.ValueKind != JsonValueKind.Object) continue;
                if (!entry.TryGetProperty("drawing_id", out var idElement)
                    || idElement.ValueKind != JsonValueKind.String) continue;
                var id = idElement.GetString();
                if (string.IsNullOrEmpty(id)) continue;
                if (!entry.TryGetProperty("count", out var countElement)
                    || countElement.ValueKind != JsonValueKind.Number
                    || !countElement.TryGetInt32(out var count)
                    || count < 0) continue;
                result.Add(new PendingDrawing(id!, count));
            }
        }
        catch (Exception)
        {
            // Unreachable in practice; an unreadable report is an empty one.
            result.Clear();
        }
        return result;
    }

    // Which error a ReplayClose for a drawing answers with, or null when the
    // close can be replayed. Only a drawing that is actually gone is "no longer open".
    public static string? CheckReplayTarget(bool found, bool isActive) =>
        !found ? "The drawing is no longer open."
        : !isActive ? "Switch to that drawing and close it again."
        : null;

    public static string BuildEventsResponse(FeedPage page) =>
        JsonSerializer.Serialize(new
        {
            type = "Events",
            payload = new
            {
                head = page.Head,
                reset = page.Reset,
                events = page.Events.Select(e => new { seq = e.Seq, type = e.Type, payload = e.Payload })
            }
        });

    public static string BuildActiveDrawingResponse(ActiveDrawingState? state, long? knownRevision)
    {
        if (state is null)
            return JsonSerializer.Serialize(new { type = "NoActiveDrawing" });
        if (knownRevision == state.Revision)
            return JsonSerializer.Serialize(new { type = "ActiveDrawingUnchanged" });
        return JsonSerializer.Serialize(new
        {
            type = "ActiveDrawing",
            payload = new
            {
                drawing_id = state.DrawingId,
                display_name = state.DisplayName,
                layer_fingerprint = state.Fingerprint,
                revision = state.Revision
            }
        });
    }
}
