// Startup fixture: supports inspection only. Any model download or inference fails and is logged.
using System;
using System.IO;
using System.Reflection;
class BootstrapFixture {
    static int Main(string[] a) {
        string args = string.Join(" ", a);
        string root = Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location);
        File.AppendAllText(Path.Combine(root,"calls.txt"), args + "\n");
        if (Path.GetFileNameWithoutExtension(Assembly.GetExecutingAssembly().Location)=="broken") { Console.WriteLine("{}"); return 0; }
        if (Array.IndexOf(a,"--probe") >= 0) Console.WriteLine("{\"models\":[\"tiny\"],\"availability\":{\"tiny\":\"download_on_first_use\"},\"gpu\":false}");
        else if (args == "-version") Console.WriteLine("ffmpeg fixture version");
        else if (args == "models --json") Console.WriteLine("{\"models\":[{\"name\":\"haas-ja-v1.0\",\"task\":\"recognition\",\"type\":\"q8_0\",\"files\":[{\"type\":\"q8_0\",\"fetched\":false}]}]}");
        else if (args == "devices --json") Console.WriteLine("{\"devices\":[]}");
        else if (args == "--json doctor") Console.WriteLine("{\"features\":{\"asr\":true,\"backend_vulkan\":false}}");
        else if (args == "--json model list") Console.WriteLine("{\"models\":[{\"repo\":\"nvidia/nemotron-3.5-asr-streaming-0.6b\"}]}");
        else return 88;
        return 0;
    }
}
