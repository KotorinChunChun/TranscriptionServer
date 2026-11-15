// Deterministic transport fixture: no model weights or real recognition.
using System;
using System.IO;
using System.Reflection;
using System.Threading;
using System.Collections.Generic;
using System.Web.Script.Serialization;
class StreamingRuntime {
    static string root=Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location);
    static JavaScriptSerializer json=new JavaScriptSerializer();
    static void Emit(object value){Console.WriteLine(json.Serialize(value));Console.Out.Flush();}
    static int Main(string[] args){
        if(args.Length>0&&args[0]=="-version"){Console.WriteLine("ffmpeg/ffprobe version fixture");return 0;}
        if(args.Length>0&&args[0]=="--probe"){
            Emit(new {models=new[]{"tiny","base"},requestOptions=new[]{"language","prompt","temperature"},gpu=true,gpuComputeType="float32",availability=new {tiny="installed",@base="installed"}});return 0;
        }
        if(args.Length>0&&args[0]=="-v"){Emit(new {format=new {duration="2"}});return 0;}
        if(args.Length>0)return 0;
        string line;
        while((line=Console.ReadLine())!=null){
            var config=json.Deserialize<Dictionary<string,object>>(line);
            var device=(string)config["device"];
            Emit(new {type="ready",device=device=="gpu"?"cuda":"cpu",computeType="float32",version="transport-fixture"});
            if(config.ContainsKey("action")){Emit(new {type="done"});continue;}
            string id=Path.GetFileName(Path.GetDirectoryName((string)config["source"]));
            File.WriteAllText(Path.Combine(root,id+".started"),device);
            Emit(new {type="language",language="ja"});
            Emit(new {type="segment",segment=new {start=0.0,end=0.2,text=id+":part1",speaker="",timestampType="whisper_segment",avg_logprob=-0.1,compression_ratio=1.0,no_speech_prob=0.0,seek=0,temperature=0.0,tokens=new[]{1,2}}});
            int count=0;
            while(File.Exists(Path.Combine(root,"hold"))){if(count++>10000)return 1;Thread.Sleep(20);}
            Emit(new {type="segment",segment=new {start=0.2,end=0.4,text=id+":part2",speaker="",timestampType="whisper_segment",avg_logprob=-0.1,compression_ratio=1.0,no_speech_prob=0.0,seek=0,temperature=0.0,tokens=new[]{3,4}}});
            Emit(new {type="done"});
        }
        return 0;
    }
}
