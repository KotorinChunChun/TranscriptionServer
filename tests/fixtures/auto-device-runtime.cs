// Deterministic fake CUDA worker + ffprobe for HTTP failover tests. No real GPU.
using System;
using System.IO;
using System.Reflection;
using System.Threading;
using System.Web.Script.Serialization;
class Fixture {
    static string root=Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location);
    static JavaScriptSerializer json=new JavaScriptSerializer();
    static void Emit(object value){Console.WriteLine(json.Serialize(value));Console.Out.Flush();}
    static int Main(string[] a){
        if(a.Length>0&&a[0]=="-version"){Console.WriteLine("ffmpeg/ffprobe version fixture");return 0;}
        if(a.Length>0&&a[0]=="--probe"){
            bool gpu=!File.Exists(Path.Combine(root,"no-gpu"));
            Emit(new {models=new[]{"tiny"},gpu=gpu,gpuComputeType=gpu?"float32":null,availability=new {tiny="installed"}});return 0;
        }
        if(a.Length>0&&a[0]=="-v"){Emit(new {format=new {duration="10"}});return 0;}
        if(a.Length>0)return 0;
        string line;
        while((line=Console.ReadLine())!=null){
            var c=json.Deserialize<System.Collections.Generic.Dictionary<string,object>>(line);
            bool load=c.ContainsKey("action");bool gpu=(string)c["device"]=="gpu";
            string mode=load?(File.Exists(Path.Combine(root,"preload-fail"))?"gpu-fail":"normal"):File.ReadAllText((string)c["source"]);
            File.AppendAllText(Path.Combine(root,"calls.txt"),mode+"\t"+(gpu?"gpu":"cpu")+"\n");
            Emit(new {type="ready",device=gpu?"cuda":"cpu",computeType=gpu?"float32":"int8",version="fixture",modelReused=false,modelLoadSeconds=0.01});
            if(mode=="always-fail"||(gpu&&mode=="gpu-fail")){Emit(new {type="error",message=gpu?"CUDA unavailable":"CPU failed"});continue;}
            if(load){Emit(new {type="done"});continue;}
            double start=Convert.ToDouble(c["start"]),end=Convert.ToDouble(c["end"]);
            Emit(new {type="segment",segment=new {start=start,end=start+1,text=gpu?"gpu transcript":"cpu transcript",speaker="",timestampType="whisper_segment"}});
            if(mode=="slow")Thread.Sleep(30000);
            if(gpu&&mode=="partial-fail"){Thread.Sleep(250);Emit(new {type="error",message="CUDA device lost after partial"});continue;}
            Emit(new {type="segment",segment=new {start=start+1,end=start+2,text=gpu?"gpu second":"cpu second",speaker="",timestampType="whisper_segment"}});
            Emit(new {type="progress",processedUntil=end});Emit(new {type="done"});
        }
        return 0;
    }
}
