namespace AcLayerStandardizer.Core;

// Layer Herder's own window and message texts. The shared kit takes these as
// arguments, so the kit stays free of product wording.
public static class LayerHerderWindow
{
    // The window's name in launch-failure messages. Kept as it has always read.
    public const string WindowName = "Layer Standardizer";

    // The Rust window's program file, as shipped beside each era's DLL.
    public const string ProgramFile = "acad_layer_ui.exe";

    public const string AlreadyOpenMessage =
        "The Layer Herder window is already open; it follows the active drawing.";
}
