// Isolated worker/ffprobe fixture. Each request writes its own barrier marker;
// request-specific text proves streams/results never cross between workers.
using System;
using System.IO;
using System.Reflection;
using System.Threading;
using System.Web.Script.Serialization;
class ConcurrentFixture {
    static string root=Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location);
    static JavaScriptSerializer json=new JavaScriptSerializer();
    static void Emit(object v){Console.WriteLine(json.Serialize(v));Console.Out.Flush();}
    static int Main(string[] a){
        if(a.Length>0&&a[0]=="-version"){Console.WriteLine("ffmpeg/ffprobe version fixture");return 0;}
        if(a.Length>0&&a[0]=="--probe"){
            Emit(new {models=new[]{"tiny","base"},gpu=true,gpuComputeType="float32",availability=new {tiny="installed",@base="installed"}});return 0;
        }
        if(a.Length>0&&a[0]=="-v"){
            Emit(new {format=new {duration=File.ReadAllText(a[a.Length-1]).StartsWith("long")?"75":"10"}});return 0;
        }
        if(a.Length>0)return 0;
        string line;
        while((line=Console.ReadLine())!=null){
            var c=json.Deserialize<System.Collections.Generic.Dictionary<string,object>>(line);
            string model=(string)c["model"],device=(string)c["device"];
            Emit(new {type="ready",device=device=="gpu"?"cuda":"cpu",computeType="float32",version="fixture"});
            if(c.ContainsKey("action")){Emit(new {type="done"});continue;}
            bool recovery=File.Exists(Path.Combine(root,"recovery-mode"));
            string mode=recovery?"recovery-"+Path.GetFileName(Path.GetDirectoryName((string)c["source"])):File.ReadAllText((string)c["source"]);
            double start=Convert.ToDouble(c["start"]),end=Convert.ToDouble(c["end"]);
            if(recovery)Emit(new {type="segment",segment=new {start=start,end=start+0.5,text=mode+"/partial",speaker="",timestampType="whisper_segment"}});
            File.WriteAllText(Path.Combine(root,mode+".started"),System.Diagnostics.Process.GetCurrentProcess().Id.ToString());
            while(!File.Exists(Path.Combine(root,"release")))Thread.Sleep(20);
            Emit(new {type="segment",segment=new {start=recovery?start+0.5:start,end=start+1,text=mode+"/"+model+"/"+device,speaker="",timestampType="whisper_segment"}});
            Emit(new {type="progress",processedUntil=end});Emit(new {type="done"});
        }
        return 0;
    }
}
