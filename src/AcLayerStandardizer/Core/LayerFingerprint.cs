using System.Collections.Generic;
using System.Linq;
using System.Security.Cryptography;
using System.Text;

namespace AcLayerStandardizer.Core;

public static class LayerFingerprint
{
    // Order- and case-insensitive digest of the layers the tool can edit, so
    // "did the layer set change?" is a cheap string comparison.
    public static string Compute(IEnumerable<string> layerNames)
    {
        var canonical = layerNames
            .Where(name => !LayerHelper.ShouldSkip(name))
            .Select(name => name.ToUpperInvariant())
            .Distinct()
            .OrderBy(name => name, StringComparer.Ordinal);

        using var sha = SHA256.Create();
        var hash = sha.ComputeHash(Encoding.UTF8.GetBytes(string.Join("\n", canonical)));
        return BitConverter.ToString(hash).Replace("-", "").ToLowerInvariant().Substring(0, 16);
    }
}
