using Xunit;
using AcLayerStandardizer.Core;

namespace AcLayerStandardizer.Tests;

public class WindowInstanceTests
{
    [Fact]
    public void a_running_window_is_brought_forward_instead_of_launching_another()
    {
        Assert.Equal(LaunchDecision.FocusExisting, WindowInstance.Decide(processAlive: true));
    }

    [Fact]
    public void with_no_running_window_a_new_one_is_launched()
    {
        Assert.Equal(LaunchDecision.Launch, WindowInstance.Decide(processAlive: false));
    }

    [Fact]
    public void the_already_open_message_says_the_window_follows_the_active_drawing()
    {
        Assert.Equal(
            "The Layer Herder window is already open; it follows the active drawing.",
            LayerHerderWindow.AlreadyOpenMessage);
    }
}
