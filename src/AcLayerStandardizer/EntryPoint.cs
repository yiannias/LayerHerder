using System.IO;
using System.Windows.Threading;
using Autodesk.AutoCAD.ApplicationServices;
using Autodesk.AutoCAD.Runtime;
using AcLayerStandardizer.Core;

[assembly: ExtensionApplication(typeof(AcLayerStandardizer.EntryPoint))]

namespace AcLayerStandardizer;

public class EntryPoint : IExtensionApplication
{
    private DispatcherTimer? _ribbonTimer;

    public void Initialize()
    {
        var config = PluginConfig.Load();

        if (string.IsNullOrWhiteSpace(config.MemoryFilePath))
        {
            config.MemoryFilePath = Path.Combine(
                PluginConfig.ConfigDirectory, "standards_memory.json");
            config.Save();
        }

        if (config.InstallRibbon)
        {
            _ribbonTimer = new DispatcherTimer
            {
                Interval = TimeSpan.FromSeconds(1)
            };
            _ribbonTimer.Tick += OnRibbonTimer;
            _ribbonTimer.Start();
        }

        LayerHerderHost.SetupMenu(config);

        try
        {
            LayerHerderHost.Install();
            IpcBridgeServer.Start();
            ActiveDrawingTracker.Start();
        }
        catch (System.Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"Failed to start IPC Bridge Server: {ex.Message}");
        }

        System.Diagnostics.Debug.WriteLine("AcLayerStandardizer loaded.");
    }

    private void OnRibbonTimer(object? sender, EventArgs e)
    {
        try
        {
            var config = PluginConfig.Load();
            if (LayerHerderHost.SetupRibbon(config))
            {
                _ribbonTimer?.Stop();
                _ribbonTimer = null;
            }
        }
        catch
        {
        }
    }

    public void Terminate()
    {
        try
        {
            ActiveDrawingTracker.Stop();
            IpcBridgeServer.Stop();
        }
        catch { }

        _ribbonTimer?.Stop();
        _ribbonTimer = null;
    }
}
