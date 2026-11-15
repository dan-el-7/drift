using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;

namespace Drift.Core;

/// The Rust core (drift_ffi.dll) behind a two-function C ABI:
///   drift_start(config_json, callback, user) -> handle   (events arrive as UTF-8 JSON on core threads)
///   drift_command(handle, json) -> 0 on success
///   drift_stop(handle)
public sealed unsafe partial class NativeCore : ICore
{
    private const string Dll = "drift_ffi";

    [LibraryImport(Dll)] private static partial nint drift_start(byte* config, delegate* unmanaged<byte*, nuint, nint, void> cb, nint user);
    [LibraryImport(Dll)] private static partial int drift_command(nint handle, byte* json);
    [LibraryImport(Dll)] private static partial void drift_stop(nint handle);

    public event Action<CoreEvent>? Event;
    private nint _handle;
    private GCHandle _self;

    public static NativeCore? TryCreate()
    {
        try
        {
            return NativeLibrary.TryLoad(Path.Combine(AppContext.BaseDirectory, Dll + ".dll"), out _) ? new NativeCore() : null;
        }
        catch { return null; }
    }

    [UnmanagedCallersOnly]
    private static void OnEvent(byte* json, nuint len, nint user)
    {
        var self = (NativeCore?)GCHandle.FromIntPtr(user).Target;
        var e = CoreEvent.Parse(Encoding.UTF8.GetString(json, (int)len));
        if (self != null && e != null) self.Event?.Invoke(e);
    }

    private static byte[] Z(string s) => Encoding.UTF8.GetBytes(s + "\0");

    public void Start(string deviceName, string downloadDir)
    {
        _self = GCHandle.Alloc(this);
        var cfg = Z(JsonSerializer.Serialize(new { device_name = deviceName, download_dir = downloadDir }));
        fixed (byte* p = cfg) _handle = drift_start(p, &OnEvent, GCHandle.ToIntPtr(_self));
        if (_handle == 0) Event?.Invoke(new CoreEvent { T = "warning", Text = "Drift's engine failed to start." });
    }

    private void Command(object c)
    {
        if (_handle == 0) return;
        var b = Z(JsonSerializer.Serialize(c));
        fixed (byte* p = b) drift_command(_handle, p);
    }

    public void Respond(ulong share, bool accept) => Command(new { c = "respond", share, accept });
    public void Scan(bool on) => Command(new { c = "scan", on });
    public void Send(string deviceId, IReadOnlyList<string> paths, IReadOnlyList<(string kind, string body)>? texts = null) =>
        Command(new { c = "send", device = deviceId, paths, texts = (texts ?? []).Select(t => new { t.kind, t.body }).ToArray() });
    public void Cancel(ulong share) => Command(new { c = "cancel", share });
    public void SetVisible(bool everyone) => Command(new { c = "visibility", everyone });
    public void Trust(ulong share) => Command(new { c = "trust", share });
    public void Untrust(string id) => Command(new { c = "untrust", id });
    public void Configure(string deviceName, string downloadDir) => Command(new { c = "settings", device_name = deviceName, download_dir = downloadDir });

    public void Dispose()
    {
        if (_handle != 0) { drift_stop(_handle); _handle = 0; }
        if (_self.IsAllocated) _self.Free();
    }
}
