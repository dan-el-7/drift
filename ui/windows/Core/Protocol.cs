using System.Text.Json;
using System.Text.Json.Serialization;

namespace Drift.Core;

// The UI <-> core protocol: the core emits one JSON object per event; the UI sends one JSON
// object per command. The same shapes come from the Rust core (drift_ffi) and from MockCore,
// so the UI is designed against exactly what ships. Field names are snake_case.

public sealed record FileInfo(string Name, long Size);

/// A trusted Drift device (Settings > Trusted devices).
public sealed record TrustedDevice(string Id, string Name, bool Auto);

public sealed record CoreEvent
{
    /// ready | warning | device_found | device_lost | incoming | waiting | accepted | medium |
    /// progress | saved | text | done
    public string T { get; init; } = "";
    public ulong Share { get; init; }
    public string? Id { get; init; }
    public string? Name { get; init; }
    /// phone | tablet | laptop | unknown
    public string? Kind { get; init; }
    public string? From { get; init; }
    public string? Pin { get; init; }
    public FileInfo[]? Files { get; init; }
    public int Texts { get; init; }
    /// url | text | address | phone (incoming: the first text item; text: this one)
    public string? TextKind { get; init; }
    /// incoming (phone -> us) or outgoing (us -> phone)
    public string? Direction { get; init; }
    /// bluetooth | lan | hotspot | direct | phone_hosted
    public string? Medium { get; init; }
    public long SetupMs { get; init; }
    public long Done { get; init; }
    public long Total { get; init; }
    public double Bps { get; init; }
    public string? Path { get; init; }
    public string? Text { get; init; }
    /// warning: one line under the title (the notice card)
    public string? Detail { get; init; }
    /// warning: a problem the UI can offer to fix (quick_share)
    public string? Code { get; init; }
    /// incoming/device_found: it runs Drift; incoming: a trusted Drift device / accepted without asking
    public bool Drift { get; init; }
    public bool Trusted { get; init; }
    public bool Auto { get; init; }
    /// trusted: the trusted devices list
    public TrustedDevice[]? Devices { get; init; }
    /// completed | declined | cancelled | failed | stalled
    public string? Outcome { get; init; }
    public string? Error { get; init; }

    public static readonly JsonSerializerOptions Json = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    };

    public static CoreEvent? Parse(string json)
    {
        try { return JsonSerializer.Deserialize<CoreEvent>(json, Json); }
        catch (JsonException) { return null; }
    }
}

public interface ICore : IDisposable
{
    /// Raised on a background thread; marshal to the UI thread.
    event Action<CoreEvent>? Event;
    void Start(string deviceName, string downloadDir);
    void Respond(ulong share, bool accept);
    void Scan(bool on);
    /// Files and/or texts (kind: url | text | address | phone).
    void Send(string deviceId, IReadOnlyList<string> paths, IReadOnlyList<(string kind, string body)>? texts = null);
    void Cancel(ulong share);
    /// Visible to everyone nearby (Quick Share's 10-minute "Everyone"), or hidden.
    void SetVisible(bool everyone);
    /// New device name / download folder, applied at once.
    void Configure(string deviceName, string downloadDir);
    /// Trust the device on the other side of a share (Drift devices, verified in that session).
    void Trust(ulong share);
    void Untrust(string id);
}
