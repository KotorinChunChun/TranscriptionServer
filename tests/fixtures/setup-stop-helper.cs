using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Threading;
using System.Web.Script.Serialization;

class SetupStopFixture {
    static void Report(string file, string state, string message) {
        File.WriteAllText(Path.ChangeExtension(file, ".txt"), message, new UTF8Encoding(false));
        var row = new { state = state, helperProcessId = Process.GetCurrentProcess().Id, message = message };
        File.WriteAllText(file, new JavaScriptSerializer().Serialize(row), new UTF8Encoding(false));
    }
    static int Main(string[] args) {
        var file = args[0]; var mode = args[1];
        Report(file, "waiting", "受付済み処理の完了を待っています。");
        Thread.Sleep(mode == "long" ? 3500 : 1000);
        if (mode == "crash") return 7;
        if (mode == "error") {
            Report(file, "error", "モデルの処理中です。\r\n安全に停止できません。"); return 1;
        }
        if (mode == "cancel") {
            File.WriteAllText(Path.ChangeExtension(file, ".cancel"), "cancel");
            Report(file, "error", "更新を取り消しました。"); return 1;
        }
        Report(file, "complete", "対象サーバーの停止を確認しました。");
        // A terminal report does not mean the actual helper process has exited yet.
        Thread.Sleep(900);
        File.WriteAllText(Path.ChangeExtension(file, ".exiting"), "now");
        return 0;
    }
}
