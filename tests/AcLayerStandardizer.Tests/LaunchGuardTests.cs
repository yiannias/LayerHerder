using Xunit;
using AcLayerStandardizer.Core;

namespace AcLayerStandardizer.Tests;

public class LaunchGuardTests
{
    private static string? Describe(LaunchOutcome outcome, string? detail) =>
        LaunchGuard.DescribeFailure(outcome, detail, LayerHerderWindow.WindowName, LayerHerderWindow.ProgramFile);

    [Fact]
    public void Success_and_already_open_need_no_message()
    {
        Assert.Null(Describe(LaunchOutcome.Launched, null));
        Assert.Null(Describe(LaunchOutcome.AlreadyOpen, "ignored"));
    }

    [Fact]
    public void A_missing_program_file_message_names_where_it_looked_and_says_to_reinstall()
    {
        Assert.Equal(
            "The Layer Standardizer window could not start: its program file (acad_layer_ui.exe) was not found " +
            @"(looked in: C:\Plugins\Contents\R25). Please reinstall the Layer Standardizer.",
            Describe(LaunchOutcome.ExecutableNotFound, @"C:\Plugins\Contents\R25"));
    }

    [Fact]
    public void A_failed_start_message_includes_the_reason()
    {
        Assert.Equal(
            "The Layer Standardizer window could not start (Access is denied). Please reinstall the Layer Standardizer.",
            Describe(LaunchOutcome.StartFailed, "Access is denied"));
        Assert.Equal(
            "The Layer Standardizer window could not start (unknown). Please reinstall the Layer Standardizer.",
            Describe(LaunchOutcome.StartFailed, null));
    }

    [Fact]
    public void A_window_that_is_not_ready_says_to_try_again()
    {
        Assert.Equal(
            "The Layer Standardizer window could not start (AutoCAD's main window was not found). Try the command again.",
            Describe(LaunchOutcome.NotReady, "AutoCAD's main window was not found"));
        Assert.Equal(
            "The Layer Standardizer window could not start (unknown). Try the command again.",
            Describe(LaunchOutcome.NotReady, null));
    }
}
