// Local HTTP integration fixture. Any ASR invocation fails and is recorded.
using System;
using System.IO;
using System.Reflection;
using System.Threading;
using System.Web.Script.Serialization;
class Fixture {
    static int Main(string[] a) {
        string root = Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location);
        string weights = Path.Combine(root,"model.gguf");
        var json = new JavaScriptSerializer();
        if(a.Length==0) return 89;
        if(a[0]=="-version"){Console.WriteLine("ffmpeg/ffprobe version fixture");return 0;}
        if(a[0]=="models") {
            Func<string,string,object> row=(name,task)=>new {name,task,type="q8_0",files=new[]{new {type="q8_0",path=weights,size=8,fetched=File.Exists(weights)}}};
            Console.WriteLine(json.Serialize(new {models=new[]{row("nemotron-3-diarization","diarization"),row("haas-ja-v1.0","recognition"),row("silero-vad","detection")}}));
            return 0;
        }
        if(a[0]=="devices") {Console.WriteLine("{\"devices\":[{\"kind\":\"gpu\"}]}");return 0;}
        if(a[0]=="-v") {Console.WriteLine("{\"format\":{\"duration\":\"10\"}}");return 0;}
        if(a[0]=="-hide_banner") {
            int i=Array.IndexOf(a,"-i");File.Copy(a[i+1],a[a.Length-1],true);return 0;
        }
        if(a[0]=="diarize") {
            File.AppendAllText(Path.Combine(root,"calls.txt"),string.Join("\t",a)+"\n");
            // Model must be an already fetched file, never a registry name that could download.
            if(a.Length<2||a[1]!=weights||!File.Exists(weights)) return 88;
            string mode=File.ReadAllText(a[a.Length-1]);
            if(mode=="slow") Thread.Sleep(30000);
            Console.WriteLine(mode=="silence"?"{\"segments\":[]}":mode=="invalid"?"{\"segments\":[{\"start\":2,\"end\":1,\"speaker\":0}]}":"{\"segments\":[{\"start\":0.25,\"end\":2.5,\"speaker\":0},{\"start\":1.5,\"end\":4,\"speaker\":1}]}");
            return 0;
        }
        File.AppendAllText(Path.Combine(root,"calls.txt"),"UNEXPECTED_ASR\t"+string.Join("\t",a)+"\n");
        return 87;
    }
}
