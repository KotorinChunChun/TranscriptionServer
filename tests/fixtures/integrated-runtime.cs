// Synthetic protocol fixture; no model download or real speech inference.
using System;
using System.IO;
using System.Text;
using System.Reflection;
using System.Collections.Generic;
using System.Web.Script.Serialization;
class Fixture {
    static string root=Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location);
    static JavaScriptSerializer json=new JavaScriptSerializer();
    static void Emit(object value){Console.WriteLine(json.Serialize(value));Console.Out.Flush();}
    static int Main(string[] a){
        Console.OutputEncoding=new UTF8Encoding(false);
        string weights=Path.Combine(root,"model.gguf");
        if(a[0]=="-version"){Console.WriteLine("ffmpeg/ffprobe version fixture");return 0;}
        if(a[0]=="models"){
            if(File.Exists(Path.Combine(root,"broken-runtime.flag")))return 86;
            Emit(new {models=new[]{new {name="nemotron-3.5-transcribe",task="recognition",type="iq4_nl",files=new[]{new {type="iq4_nl",path=weights,size=8,fetched=File.Exists(weights)}}}}});return 0;
        }
        if(a[0]=="devices"){Emit(new {devices=new[]{new {kind="igpu"}}});return 0;}
        if(a[0]=="-v"){Emit(new {format=new {duration="10"}});return 0;}
        if(a[0]=="-hide_banner"){
            using(var f=new BinaryWriter(File.Create(a[a.Length-1]))){
                int size=3*32000;f.Write(Encoding.ASCII.GetBytes("RIFF"));f.Write(size+36);f.Write(Encoding.ASCII.GetBytes("WAVEfmt "));f.Write(16);f.Write((short)1);f.Write((short)1);f.Write(16000);f.Write(32000);f.Write((short)2);f.Write((short)16);f.Write(Encoding.ASCII.GetBytes("data"));f.Write(size);f.Write(new byte[size]);
            }return 0;
        }
        if(a[0]!="worker"||a[1]!=weights||!File.Exists(weights)||Array.IndexOf(a,"cpu")<0)return 88;
        File.AppendAllText(Path.Combine(root,"loads.txt"),"load\n");
        Emit(new {type="ready",protocol=4,version="0.10.1"});
        int chunks=0;string line;
        while((line=Console.ReadLine())!=null){
            var v=json.Deserialize<Dictionary<string,object>>(line);
            if((string)v["type"]=="chunk"){if(Convert.ToInt32(v["seq"])!=chunks++)return 89;continue;}
            if((string)v["type"]!="transcribe"||chunks!=2||!(bool)v["timestamps"])return 90;
            chunks=0;bool speakers=(bool)v["speakers"];
            string mode=File.Exists(Path.Combine(root,"mode.txt"))?File.ReadAllText(Path.Combine(root,"mode.txt")):"normal";
            if(mode=="untimed")Emit(new {type="end",id=v["id"],text="saved untimed text",untimed="alignment_failed",stop="complete"});
            else Emit(new {type="end",id=v["id"],text="first second",segments=new[]{new {start=0.25,end=2.0,text="first",speaker=speakers?(object)0:null},new {start=1.5,end=2.5,text="second",speaker=speakers?(object)1:null}},stop="complete"});
        }return 0;
    }
}
