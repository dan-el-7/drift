using Microsoft.Windows.AppNotifications;
using Microsoft.Windows.AppNotifications.Builder;

namespace Drift.Shell;

/// Windows notifications, used only while Drift isn't in front: a phone wants to send (Accept /
/// Decline right there), a file was saved (Open / Show in folder), a transfer failed.
/// Buttons come back through <see cref="Invoked"/> as key/value arguments, on a background thread.
public sealed class Notices : IDisposable
{
    public event Action<IDictionary<string, string>>? Invoked;
    private readonly bool _ok;

    public Notices()
    {
        try
        {
            AppNotificationManager.Default.NotificationInvoked += (_, e) => Invoked?.Invoke(e.Arguments);
            AppNotificationManager.Default.Register();
            _ok = true;
        }
        catch (Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"Drift: notifications unavailable: {ex.Message}");
        }
    }

    private void Show(AppNotificationBuilder b, string tag)
    {
        if (!_ok) return;
        try
        {
            var n = b.BuildNotification();
            n.Tag = tag; n.Group = "drift";
            AppNotificationManager.Default.Show(n);
        }
        catch (Exception ex) { System.Diagnostics.Debug.WriteLine($"Drift: notification failed: {ex.Message}"); }
    }

    public void Incoming(ulong share, string from, string what, string? pin) => Show(new AppNotificationBuilder()
            .AddArgument("act", "open").AddArgument("share", share.ToString())
            .AddText($"{from} wants to send")
            .AddText(pin != null ? $"{what} · code {pin}" : what)
            .AddButton(new AppNotificationButton("Accept").AddArgument("act", "accept").AddArgument("share", share.ToString()))
            .AddButton(new AppNotificationButton("Decline").AddArgument("act", "decline").AddArgument("share", share.ToString())),
        $"in-{share}");

    public void Saved(ulong share, string title, string? path) => Show(new AppNotificationBuilder()
            .AddArgument("act", "open").AddArgument("share", share.ToString())
            .AddText($"Saved {title}")
            .AddButton(new AppNotificationButton("Open").AddArgument("act", "openfile").AddArgument("share", share.ToString()))
            .AddButton(new AppNotificationButton("Show in folder").AddArgument("act", "folder").AddArgument("share", share.ToString())),
        $"in-{share}");

    public void TextReceived(ulong share, string title, string content, bool isLink)
    {
        var b = new AppNotificationBuilder()
            .AddArgument("act", "open").AddArgument("share", share.ToString())
            .AddText(title)
            .AddText(content.Length > 120 ? content[..120] + "…" : content);
        if (isLink) b.AddButton(new AppNotificationButton("Open").AddArgument("act", "openlink").AddArgument("share", share.ToString()));
        b.AddButton(new AppNotificationButton("Copy").AddArgument("act", "copy").AddArgument("share", share.ToString()));
        Show(b, $"in-{share}");
    }

    public void Failed(string text) => Show(new AppNotificationBuilder().AddArgument("act", "open").AddText(text), "fail");

    /// The share was answered in the window: take its notification away.
    public async void Remove(ulong share)
    {
        if (!_ok) return;
        try { await AppNotificationManager.Default.RemoveByTagAndGroupAsync($"in-{share}", "drift"); } catch { }
    }

    public void Dispose()
    {
        if (!_ok) return;
        try { AppNotificationManager.Default.Unregister(); } catch { }
    }
}
