// Synthetic weights and protocol only. Never downloads models.
using System;
using System.IO;
using System.Reflection;
using System.Web.Script.Serialization;
class Fixture {
    static string root=Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location);
    static void Emit(object value){Console.WriteLine(new JavaScriptSerializer().Serialize(value));Console.Out.Flush();}
    static object Row(string name,string task){return new{name=name,task=task,type="q8_0",files=new[]{new{type="q8_0",fetched=File.Exists(Path.Combine(root,name+".weights"))}}};}
    static int Main(string[] a){
        if(a[0]=="-version"){Console.WriteLine("ffmpeg/ffprobe version fixture");return 0;}
        if(a[0]=="models"){Emit(new{models=new[]{Row("haas-ja-v1.0","recognition"),Row("silero-vad","vad")}});return 0;}
        if(a[0]=="devices"){Emit(new{devices=new object[0]});return 0;}
        if((a[0]=="pull"||a[0]=="rm")&&(a[1]=="haas-ja-v1.0"||a[1]=="silero-vad")){
            File.AppendAllText(Path.Combine(root,"operations.txt"),a[0]+" "+a[1]+"\n");
            if(a[0]=="pull" && File.Exists(Path.Combine(root,"slow-pull"))){Console.Error.Write("\rDownloading 25%");Console.Error.Flush();System.Threading.Thread.Sleep(5000);}
            string file=Path.Combine(root,a[1]+".weights");
            if(a[0]=="pull")File.WriteAllText(file,"fixture");else File.Delete(file);
            return 0;
        }
        if(a[0]=="worker"&&a[1]=="haas-ja-v1.0"){
            Emit(new{type="ready",protocol=4});while(Console.ReadLine()!=null){}return 0;
        }
        return 88;
    }
}
