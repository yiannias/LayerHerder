using System.IO;
using Autodesk.AutoCAD.Runtime;
using Autodesk.AutoCAD.ApplicationServices;
using AcLayerStandardizer.Core;

namespace AcLayerStandardizer.Commands;

public static class MappingsCommand
{
    // The single handler for every entry point (ribbon/menu send LSR).
    [CommandMethod("LSTDR")]
    [CommandMethod("HERD", CommandFlags.Modal)]
    [CommandMethod("ACLAYERSTD", "STD_Mappings", CommandFlags.Modal)]
    [CommandMethod("ACLAYERSTD", "StandardizeLayers", CommandFlags.Modal)]
    [CommandMethod("LAYERSTANDARDIZER", CommandFlags.Modal)]
    [CommandMethod("ACLAYERSTD", "LAYERSTANDARDIZER", CommandFlags.Modal)]
    public static void ShowMappingsEditor()
    {
        var doc = Application.DocumentManager.MdiActiveDocument;
        if (doc is null) return;

        var ed = doc.Editor;

        // The Rust window is the only editor: it reads the template, categorizes
        // and loads memory itself, so only the active drawing's layers are read
        // here. A failure is printed at the command line; nothing is thrown.
        try
        {
            // The window is already open: bring it forward before anything else.
            if (RustUiLauncher.TryFocusExistingWindow(doc)) return;

            var config = PluginConfig.Load();

            var templatePath = !string.IsNullOrEmpty(config.TemplateDwgPath) && File.Exists(config.TemplateDwgPath)
                ? config.TemplateDwgPath
                : string.Empty;
            if (templatePath.Length == 0)
                ed.WriteMessage("\nReference file unavailable. Opening without targets; click the Target header to choose one.");

            var source = LayerReader.GetActiveLayerNames(doc.Database)
                .OrderBy(n => n, NaturalSortComparer.Instance).ToList();
            var outcome = RustUiLauncher.TryLaunchFromActiveAutoCad(
                doc, source, LayerReader.GetEmptyLayers(doc.Database), templatePath, out var detail);

            var message = LaunchGuard.DescribeFailure(outcome, detail, LayerHerderWindow.WindowName, LayerHerderWindow.ProgramFile);
            if (message is not null) ed.WriteMessage("\n" + message);
        }
        catch (System.Exception ex)
        {
            // The pipe server start, the drawing snapshot or the layer read failed.
            try { ed.WriteMessage($"\nThe Layer Standardizer window could not start ({ex.GetType().Name}: {ex.Message})."); }
            catch (System.Exception) { /* the command line is unavailable: nothing else to do */ }
        }
    }
}
