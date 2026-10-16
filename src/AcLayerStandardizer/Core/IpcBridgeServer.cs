using System;
using System.Collections.Generic;
using System.IO;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using Autodesk.AutoCAD.ApplicationServices;
using AcLayerStandardizer.Commands;

namespace AcLayerStandardizer.Core;

public static class IpcBridgeServer
{
    // Layer Herder's pipe name. Tests overwrite it before Start so parallel test
    // processes use private pipes.
    static IpcBridgeServer() => BridgeServer.PipeName = "acad_layer_standardizer";

    // The pipe is served by the shared kit's BridgeServer.
    public static string PipeName
    {
        get => BridgeServer.PipeName;
        set => BridgeServer.PipeName = value;
    }

    private static readonly object SnapshotLock = new();
    private static readonly object LogLock = new();
    private static DrawingSnapshot? _drawingSnapshot;

    private sealed record DrawingSnapshot(
        Document Document,
        string DrawingId,
        string DrawingName,
        string TemplateName,
        string TemplatePath,
        string[] SourceLayers,
        string[] StandardLayers,
        string[] EmptyLayers,
        IReadOnlyDictionary<string, LayerProperties> StandardLayerProperties);


    // Refusal wording for the open window: switch drawings in AutoCAD, never
    // "run LSTDR again" (that only brings the same window forward).
    private static readonly string NoDrawingReadMessage =
        "No drawing has been read yet. " + IpcProtocol.SwitchAndTryAgain(null);

    private static string NoLongerActiveMessage(string drawingName) =>
        "The drawing is no longer active in AutoCAD. " + IpcProtocol.SwitchAndTryAgain(drawingName);

    // Called from an AutoCAD command while its document context is valid. The
    // pipe worker serves this immutable snapshot and never touches AutoCAD APIs.
    // The standard's layers start empty: the window fetches them with
    // GetStandardLayers, which fills them in.
    public static void SetDrawingSnapshot(
        Document document,
        IEnumerable<string> sourceLayers,
        IEnumerable<string> emptyLayers,
        string templatePath)
    {
        var snapshot = new DrawingSnapshot(
            document,
            ActiveDrawingTracker.GetDrawingId(document),
            Path.GetFileName(document.Name),
            string.IsNullOrEmpty(templatePath) ? "" : Path.GetFileName(templatePath),
            templatePath,
            sourceLayers.ToArray(),
            Array.Empty<string>(),
            emptyLayers.ToArray(),
            new Dictionary<string, LayerProperties>(StringComparer.OrdinalIgnoreCase));
        lock (SnapshotLock) _drawingSnapshot = snapshot;
        Log($"Snapshot stored: sources={snapshot.SourceLayers.Length}; standards={snapshot.StandardLayers.Length}.");
    }
    public static bool IsRunning => BridgeServer.IsRunning;

    public static void Start()
    {
        RegisterHandlers();
        BridgeServer.Start();
    }

    public static void Stop() => BridgeServer.Stop();

    // One handler per request type. Each body is the one the old switch ran for
    // that type; the kit's server does the reading, writing and error wrapping.
    private static void RegisterHandlers()
    {
        // The window polls about once a second; those polls are answered, not logged.
        BridgeServer.QuietRequestTypes.Add("PollEvents");

        BridgeServer.Register("Ping", _ => BridgeServer.Response("Pong"));
        BridgeServer.Register("GetDrawingLayers", DrawingLayersResponse);
        BridgeServer.Register("GetActiveDrawing", ActiveDrawingResponse);
        BridgeServer.Register("PollEvents", PollEventsResponse);
        BridgeServer.Register("GetDrawingSnapshot", DrawingSnapshotResponse);
        BridgeServer.RegisterAsync("ReplayClose", ReplayCloseRequestAsync);
        BridgeServer.RegisterAsync("GetLayersForDrawing", GetLayersForDrawingRequestAsync);
        BridgeServer.RegisterAsync("GetStandardLayers", GetStandardLayersRequestAsync);
        BridgeServer.RegisterAsync("ApplyPlan", ApplyPlanRequestAsync);
        BridgeServer.RegisterAsync("PurgeEmptyLayers", PurgeEmptyLayersRequestAsync);
    }

    private static string DrawingLayersResponse(JsonElement root)
    {
        var layers = GetDrawingSnapshot()?.SourceLayers ?? Array.Empty<string>();
        return JsonSerializer.Serialize(new { type = "Layers", payload = layers });
    }

    private static string ActiveDrawingResponse(JsonElement root)
    {
        long? knownRevision = null;
        if (root.TryGetProperty("payload", out var activePayload)
            && activePayload.ValueKind == JsonValueKind.Object
            && activePayload.TryGetProperty("known_revision", out var known)
            && known.ValueKind == JsonValueKind.Number)
            knownRevision = known.GetInt64();
        return IpcProtocol.BuildActiveDrawingResponse(ActiveDrawingRegistry.Current, knownRevision);
    }

    private static string PollEventsResponse(JsonElement root)
    {
        if (CheckProtocolVersion(root) is { } versionErrorPoll) return versionErrorPoll;
        long? since = null;
        var pollPayload = root.GetProperty("payload");
        if (pollPayload.TryGetProperty("since", out var sinceElement)
            && sinceElement.ValueKind == JsonValueKind.Number)
            since = sinceElement.GetInt64();
        // Every poll is a check-in; a malformed report counts as an empty one.
        PendingRegistry.Report(IpcProtocol.ParsePending(pollPayload), DateTime.UtcNow);
        return IpcProtocol.BuildEventsResponse(EventFeed.Shared.Read(since));
    }

    private static string DrawingSnapshotResponse(JsonElement root)
    {
        var snapshot = GetDrawingSnapshot();
        if (snapshot is null) return Error(NoDrawingReadMessage);
        return SerializeSnapshot("DrawingSnapshot", snapshot);
    }

    private static async Task<string> ReplayCloseRequestAsync(JsonElement root)
    {
        if (!root.TryGetProperty("payload", out var replayPayload)
            || replayPayload.ValueKind != JsonValueKind.Object
            || !replayPayload.TryGetProperty("protocol_version", out var replayVersion)
            || replayVersion.ValueKind != JsonValueKind.Number)
            return Error("The request is missing its payload or protocol_version.");
        if (CheckProtocolVersion(root) is { } versionErrorReplay) return versionErrorReplay;
        if (!replayPayload.TryGetProperty("kind", out var kindElement)
            || kindElement.ValueKind != JsonValueKind.String
            || kindElement.GetString() is not ("drawing" or "quit"))
            return Error("The request's kind must be \"drawing\" or \"quit\".");
        var replayKind = kindElement.GetString()!;
        string? replayDrawingId = null;
        if (replayKind == "drawing")
        {
            if (!replayPayload.TryGetProperty("drawing_id", out var replayIdElement)
                || replayIdElement.ValueKind != JsonValueKind.String
                || string.IsNullOrEmpty(replayIdElement.GetString()))
                return Error("The request is missing drawing_id.");
            replayDrawingId = replayIdElement.GetString();
        }
        // The carried report no longer lists what the user just applied or
        // discarded, so the replayed close passes the close guard.
        PendingRegistry.Report(IpcProtocol.ParsePending(replayPayload), DateTime.UtcNow);
        return await ReplayCloseAsync(replayKind, replayDrawingId).ConfigureAwait(false);
    }

    private static async Task<string> GetLayersForDrawingRequestAsync(JsonElement root)
    {
        if (!root.TryGetProperty("payload", out var layersPayload)
            || layersPayload.ValueKind != JsonValueKind.Object
            || !layersPayload.TryGetProperty("protocol_version", out var layersVersion)
            || layersVersion.ValueKind != JsonValueKind.Number)
            return Error("The request is missing its payload or protocol_version.");
        if (CheckProtocolVersion(root) is { } versionErrorLayers) return versionErrorLayers;
        if (!layersPayload.TryGetProperty("drawing_id", out var layersIdElement)
            || layersIdElement.ValueKind != JsonValueKind.String
            || string.IsNullOrEmpty(layersIdElement.GetString()))
            return Error("The request is missing drawing_id.");
        return await GetLayersForDrawingAsync(layersIdElement.GetString()!).ConfigureAwait(false);
    }

    private static async Task<string> GetStandardLayersRequestAsync(JsonElement root)
    {
        if (CheckProtocolVersion(root) is { } versionErrorGetStandard) return versionErrorGetStandard;
        var standardPath = root.GetProperty("payload").GetProperty("path").GetString() ?? string.Empty;
        if (string.IsNullOrWhiteSpace(standardPath) || !File.Exists(standardPath))
            return Error("The selected standard file could not be found.");
        return await GetStandardLayersAsync(standardPath).ConfigureAwait(false);
    }

    private static async Task<string> ApplyPlanRequestAsync(JsonElement root)
    {
        if (CheckProtocolVersion(root) is { } versionErrorApplyPlan) return versionErrorApplyPlan;
        return await ApplyPlanAsync(root).ConfigureAwait(false);
    }

    private static async Task<string> PurgeEmptyLayersRequestAsync(JsonElement root)
    {
        if (CheckProtocolVersion(root) is { } versionErrorPurgeEmptyLayers) return versionErrorPurgeEmptyLayers;
        return await PurgeEmptyLayersAsync(root).ConfigureAwait(false);
    }

    private static string? CheckProtocolVersion(JsonElement root)
    {
        var version = root.GetProperty("payload").GetProperty("protocol_version").GetInt32();
        return IpcProtocol.IsSupportedVersion(version) ? null : Error($"Unsupported protocol version {version}.");
    }

    // Reads the standard (template) layer names for the Rust window and caches the
    // layers' properties in the snapshot for Apply. It neither writes config nor
    // touches memory/categories: the Rust app owns those.
    private static async Task<string> GetStandardLayersAsync(string path)
    {
        var current = GetDrawingSnapshot();
        if (current is null) return Error(NoDrawingReadMessage);

        var completion = new TaskCompletionSource<DrawingSnapshot>(TaskCreationOptions.RunContinuationsAsynchronously);
        try
        {
            await Application.DocumentManager.ExecuteInCommandContextAsync(_ =>
            {
                try
                {
                    if (!ReferenceEquals(Application.DocumentManager.MdiActiveDocument, current.Document))
                        throw new InvalidOperationException(NoLongerActiveMessage(current.DrawingName));

                    var properties = SideDatabase.LoadStandardLayers(path);
                    var names = properties.Keys.OrderBy(n => n == "0" ? 0 : 1)
                        .ThenBy(n => n, NaturalSortComparer.Instance).ToArray();
                    var updated = current with
                    {
                        TemplateName = Path.GetFileName(path),
                        TemplatePath = path,
                        StandardLayers = names,
                        StandardLayerProperties = properties
                    };
                    // Another LSTDR may have stored a newer snapshot while the standard was
                    // loading; replacing it with this one would resurrect a stale drawing.
                    lock (SnapshotLock)
                    {
                        if (!ReferenceEquals(_drawingSnapshot, current))
                            throw new InvalidOperationException("The drawing changed while the standard was loading. " + IpcProtocol.SwitchAndTryAgain(null));
                        _drawingSnapshot = updated;
                    }
                    current.Document.Editor.WriteMessage($"\nStandard loaded: {Path.GetFileName(path)} ({names.Length} layers).");
                    completion.TrySetResult(updated);
                }
                catch (Exception ex) { completion.TrySetException(ex); }
                return Task.CompletedTask;
            }, null);
            var loaded = await completion.Task.ConfigureAwait(false);
            return JsonSerializer.Serialize(new
            {
                type = "StandardLayers",
                payload = new { template_name = loaded.TemplateName, template_path = loaded.TemplatePath, layers = loaded.StandardLayers }
            });
        }
        catch (Exception ex) { return Error($"Could not load the standard file: {ex.GetBaseException().Message}"); }
    }

    private sealed class DrawingClosedException : Exception { }

    // Reads another open drawing's layers for the Rust window and makes it the
    // drawing Apply/Purge act on, keeping the standard-layer data.
    private static async Task<string> GetLayersForDrawingAsync(string drawingId)
    {
        var current = GetDrawingSnapshot();
        var completion = new TaskCompletionSource<DrawingSnapshot>(TaskCreationOptions.RunContinuationsAsynchronously);
        try
        {
            try { await QueueLayerReadAsync(drawingId, current, completion).ConfigureAwait(false); }
            catch (Exception)
            {
                // The command context itself could not run (for example no AutoCAD
                // host): no drawing can be found, so the drawing counts as closed.
                throw new DrawingClosedException();
            }
            var read = await completion.Task.ConfigureAwait(false);
            return JsonSerializer.Serialize(new
            {
                type = "DrawingLayers",
                payload = new
                {
                    drawing_id = read.DrawingId,
                    drawing_name = read.DrawingName,
                    source_layers = read.SourceLayers,
                    empty_layers = read.EmptyLayers
                }
            });
        }
        catch (DrawingClosedException) { return Error("The drawing is no longer open."); }
        catch (Exception ex) { return Error($"Could not read the drawing's layers: {ex.GetBaseException().Message}"); }
    }

    // Kept out of line so that loading the AutoCAD assemblies happens inside the
    // caller's try block (it fails in a process with no AutoCAD).
    [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)]
    private static async Task QueueLayerReadAsync(
        string drawingId, DrawingSnapshot? current, TaskCompletionSource<DrawingSnapshot> completion) =>
        await Application.DocumentManager.ExecuteInCommandContextAsync(_ =>
        {
            try
            {
                var document = ActiveDrawingTracker.TryFindDocument(drawingId);
                if (document is null) throw new DrawingClosedException();
                if (current is null)
                    throw new InvalidOperationException(NoDrawingReadMessage);

                var source = LayerReader.GetActiveLayerNames(document.Database)
                    .OrderBy(n => n, NaturalSortComparer.Instance).ToArray();
                var empty = LayerReader.GetEmptyLayers(document.Database)
                    .OrderBy(n => n, NaturalSortComparer.Instance).ToArray();
                var updated = current with
                {
                    Document = document,
                    DrawingId = drawingId,
                    DrawingName = Path.GetFileName(document.Name),
                    SourceLayers = source,
                    EmptyLayers = empty
                };
                lock (SnapshotLock)
                {
                    if (!ReferenceEquals(_drawingSnapshot, current))
                        throw new InvalidOperationException("The drawing window was replaced while its layers were being read. Try again.");
                    _drawingSnapshot = updated;
                }
                completion.TrySetResult(updated);
            }
            catch (Exception ex) { completion.TrySetException(ex); }
            return Task.CompletedTask;
        }, null);

    // Re-runs a close the close guard blocked, as a normal CLOSE or QUIT command,
    // so AutoCAD's own save prompt appears as usual.
    private static async Task<string> ReplayCloseAsync(string kind, string? drawingId)
    {
        var completion = new TaskCompletionSource<string?>(TaskCreationOptions.RunContinuationsAsynchronously);
        try
        {
            await QueueReplayCloseAsync(kind, drawingId, completion).ConfigureAwait(false);
            var error = await completion.Task.ConfigureAwait(false);
            return error is null ? JsonSerializer.Serialize(new { type = "Replayed" }) : Error(error);
        }
        catch (Exception ex)
        {
            Log($"ReplayClose ({kind}) failed: {ex.GetType().Name}: {ex.Message}");
            var what = kind == "quit" ? "quit AutoCAD" : "close the drawing";
            return Error($"Could not {what}: {ex.GetBaseException().Message}");
        }
    }

    // Kept out of line so that loading the AutoCAD assemblies happens inside the
    // caller's try block (it fails in a process with no AutoCAD). Completes with
    // an error message for the client, or null once the command is queued.
    [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)]
    private static async Task QueueReplayCloseAsync(
        string kind, string? drawingId, TaskCompletionSource<string?> completion) =>
        await Application.DocumentManager.ExecuteInCommandContextAsync(_ =>
        {
            try
            {
                var active = Application.DocumentManager.MdiActiveDocument;
                if (kind == "quit")
                {
                    if (active is null)
                    {
                        completion.TrySetResult("No drawing is active to quit from.");
                    }
                    else
                    {
                        active.SendStringToExecute("_.QUIT ", true, false, false);
                        completion.TrySetResult(null);
                    }
                }
                else
                {
                    var document = ActiveDrawingTracker.TryFindDocument(drawingId!);
                    var error = IpcProtocol.CheckReplayTarget(
                        found: document is not null,
                        isActive: document is not null && ReferenceEquals(document, active));
                    if (error is null)
                        document!.SendStringToExecute("_.CLOSE ", true, false, false);
                    completion.TrySetResult(error);
                }
            }
            catch (Exception ex) { completion.TrySetException(ex); }
            return Task.CompletedTask;
        }, null);

    private static string SerializeSnapshot(string type, DrawingSnapshot snapshot) => JsonSerializer.Serialize(new
    {
        type,
        payload = new
        {
            drawing_id = snapshot.DrawingId,
            drawing_name = snapshot.DrawingName,
            template_name = snapshot.TemplateName,
            template_path = snapshot.TemplatePath,
            source_layers = snapshot.SourceLayers,
            standard_layers = snapshot.StandardLayers,
            empty_layers = snapshot.EmptyLayers,
            // Legacy fields the Rust window still requires (or tolerates) in the
            // snapshot; it computes memory, categories and matching itself.
            heuristic_threshold = 0.6,
            memory_mappings = new Dictionary<string, string>(),
            always_hidden_targets = Array.Empty<string>(),
            target_filters = Array.Empty<object>()
        }
    });

    private static async Task<string> ApplyPlanAsync(JsonElement root)
    {
        var payload = root.GetProperty("payload");
        var version = payload.GetProperty("protocol_version").GetInt32();
        if (!IpcProtocol.IsSupportedVersion(version))
            return Error($"Unsupported apply-plan protocol version {version}.");

        var snapshot = GetDrawingSnapshot();
        if (snapshot is null)
            return Error(NoDrawingReadMessage);

        var drawingName = payload.GetProperty("drawing_name").GetString() ?? string.Empty;
        var requestDrawingId = payload.TryGetProperty("drawing_id", out var idElement) ? idElement.GetString() : null;
        var targetError = IpcProtocol.CheckDrawingTarget(snapshot.DrawingId, snapshot.DrawingName, requestDrawingId, drawingName);
        if (targetError is not null) return Error(targetError);

        var mappings = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (var item in payload.GetProperty("mappings").EnumerateArray())
        {
            var source = item.GetProperty("source_layer").GetString() ?? string.Empty;
            var target = item.GetProperty("target_layer").GetString() ?? string.Empty;
            if (string.IsNullOrWhiteSpace(source) || string.IsNullOrWhiteSpace(target))
                return Error("A mapping contains an empty layer name.");
            if (!snapshot.SourceLayers.Contains(source, StringComparer.OrdinalIgnoreCase))
                return Error($"The source layer '{source}' was not in the drawing snapshot.");
            if (!snapshot.StandardLayerProperties.ContainsKey(target))
                return Error($"The target layer '{target}' was not in the selected standard.");
            if (!mappings.TryAdd(source, target))
                return Error($"The source layer '{source}' appears more than once in the plan.");
        }

        if (mappings.Count == 0)
            return Error("There are no mappings to apply.");

        // Still required so older clients' requests stay valid, but ignored: the
        // Rust app owns translation memory, so the connector never writes it.
        _ = payload.GetProperty("remember").GetBoolean();
        var settings = payload.GetProperty("properties");
        var propertySettings = new PropertyMatchSettings(
            settings.GetProperty("match_color").GetBoolean(),
            settings.GetProperty("match_linetype").GetBoolean(),
            settings.GetProperty("match_lineweight").GetBoolean(),
            settings.GetProperty("make_by_layer").GetBoolean());

        var completion = new TaskCompletionSource<LayerApplier.ApplyMappingsResult>(
            TaskCreationOptions.RunContinuationsAsynchronously);
        try
        {
            await Application.DocumentManager.ExecuteInCommandContextAsync(_ =>
            {
                try
                {
                    var activeDocument = Application.DocumentManager.MdiActiveDocument;
                    if (!ReferenceEquals(activeDocument, snapshot.Document))
                    {
                        completion.TrySetException(new InvalidOperationException(
                            NoLongerActiveMessage(snapshot.DrawingName)));
                    }
                    else
                    {
                        var result = LayerApplier.ApplyMappings(
                            snapshot.Document.Database, mappings,
                            snapshot.StandardLayerProperties, propertySettings);

                        snapshot.Document.Editor.WriteMessage(
                            $"\nRust mappings applied: {result.Renamed} renamed/merged, {result.Synced} layer properties synced.");
                        completion.TrySetResult(result);
                    }
                }
                catch (Exception ex)
                {
                    completion.TrySetException(ex);
                }

                return Task.CompletedTask;
            }, null);

            var applied = await completion.Task.ConfigureAwait(false);
            return JsonSerializer.Serialize(new
            {
                type = "Applied",
                payload = new
                {
                    protocol_version = version,
                    count = applied.Renamed,
                    remembered = false,
                    warning = (string?)null
                }
            });
        }
        catch (Exception ex)
        {
            return Error($"Could not apply the mapping plan: {ex.GetBaseException().Message}");
        }
    }

    // Also used by ActiveDrawingTracker, so close-guard failures land in the same log.
    internal static void Log(string message)
    {
        try
        {
            lock (LogLock)
                File.AppendAllText(Path.Combine(Path.GetTempPath(), "AcLayerStandardizer-ipc.log"),
                    $"{DateTime.UtcNow:O} {message}{Environment.NewLine}");
        }
        catch { }
    }

    private static async Task<string> PurgeEmptyLayersAsync(JsonElement root)
    {
        var payload = root.GetProperty("payload");
        var version = payload.GetProperty("protocol_version").GetInt32();
        if (!IpcProtocol.IsSupportedVersion(version))
            return Error($"Unsupported purge protocol version {version}.");

        var snapshot = GetDrawingSnapshot();
        if (snapshot is null)
            return Error(NoDrawingReadMessage);

        var drawingName = payload.GetProperty("drawing_name").GetString() ?? string.Empty;
        var requestDrawingId = payload.TryGetProperty("drawing_id", out var idElement) ? idElement.GetString() : null;
        var targetError = IpcProtocol.CheckDrawingTarget(snapshot.DrawingId, snapshot.DrawingName, requestDrawingId, drawingName);
        if (targetError is not null) return Error(targetError);

        var requestedNames = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var item in payload.GetProperty("layers").EnumerateArray())
        {
            var name = item.GetString() ?? string.Empty;
            if (!snapshot.EmptyLayers.Contains(name, StringComparer.OrdinalIgnoreCase))
                return Error($"The layer '{name}' was not empty in the drawing snapshot.");
            requestedNames.Add(name);
        }

        if (requestedNames.Count == 0)
            return Error("There are no empty layers selected for purging.");

        var completion = new TaskCompletionSource<string[]>(TaskCreationOptions.RunContinuationsAsynchronously);
        try
        {
            await Application.DocumentManager.ExecuteInCommandContextAsync(_ =>
            {
                try
                {
                    var activeDocument = Application.DocumentManager.MdiActiveDocument;
                    if (!ReferenceEquals(activeDocument, snapshot.Document))
                    {
                        completion.TrySetException(new InvalidOperationException(
                            NoLongerActiveMessage(snapshot.DrawingName)));
                    }
                    else
                    {
                        var stillEmpty = LayerReader.GetEmptyLayers(snapshot.Document.Database);
                        var purged = new List<string>();
                        using var transaction = snapshot.Document.Database.TransactionManager.StartTransaction();
                        var table = (Autodesk.AutoCAD.DatabaseServices.LayerTable)transaction.GetObject(
                            snapshot.Document.Database.LayerTableId,
                            Autodesk.AutoCAD.DatabaseServices.OpenMode.ForRead);
                        foreach (var name in requestedNames)
                        {
                            if (!stillEmpty.Contains(name) || !table.Has(name)) continue;
                            var id = table[name];
                            if (id == snapshot.Document.Database.Clayer) continue;
                            var layer = (Autodesk.AutoCAD.DatabaseServices.LayerTableRecord)transaction.GetObject(
                                id, Autodesk.AutoCAD.DatabaseServices.OpenMode.ForWrite);
                            try
                            {
                                layer.Erase(true);
                                purged.Add(name);
                            }
                            catch
                            {
                                // Match the existing WPF purge behavior: skip layers AutoCAD refuses to erase.
                            }
                        }
                        transaction.Commit();
                        snapshot.Document.Editor.WriteMessage($"\nPurged {purged.Count} empty layer(s).");
                        completion.TrySetResult(purged.ToArray());
                    }
                }
                catch (Exception ex)
                {
                    completion.TrySetException(ex);
                }

                return Task.CompletedTask;
            }, null);

            var purged = await completion.Task.ConfigureAwait(false);
            return JsonSerializer.Serialize(new
            {
                type = "Purged",
                payload = new { protocol_version = version, layers = purged }
            });
        }
        catch (Exception ex)
        {
            return Error($"Could not purge empty layers: {ex.GetBaseException().Message}");
        }
    }

    private static string Error(string message) =>
        JsonSerializer.Serialize(new { type = "Error", payload = message });

    private static DrawingSnapshot? GetDrawingSnapshot()
    {
        lock (SnapshotLock) return _drawingSnapshot;
    }
}
