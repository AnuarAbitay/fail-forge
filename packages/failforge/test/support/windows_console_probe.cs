using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Web.Script.Serialization;

// Test-only executable. No process not created by this probe is terminated.
public static class ConsoleProbe
{
    const uint NEW_CONSOLE = 0x10, SUSPENDED = 4;
    const uint KILL_ON_CLOSE = 0x2000;
    static readonly JavaScriptSerializer Json = new JavaScriptSerializer();
    static int observed;
    static Handler handler;
    delegate bool Handler(uint kind);

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO {
        public uint cb; public string reserved, desktop, title;
        public uint x,y,xSize,ySize,xCount,yCount,fill,flags;
        public ushort showWindow, reserved2Size;
        public IntPtr reserved2, input, output, error;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION { public IntPtr process, thread; public uint pid, tid; }
    [StructLayout(LayoutKind.Sequential)]
    struct BASIC_LIMITS {
        public long processTime, jobTime; public uint flags;
        public UIntPtr minWorkingSet,maxWorkingSet; public uint activeProcessLimit;
        public UIntPtr affinity; public uint priorityClass,schedulingClass;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct IO_COUNTERS { public ulong a,b,c,d,e,f; }
    [StructLayout(LayoutKind.Sequential)]
    struct EXTENDED_LIMITS {
        public BASIC_LIMITS basic; public IO_COUNTERS io;
        public UIntPtr processMemory,jobMemory,peakProcessMemory,peakJobMemory;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct ACCOUNTING {
        public long userTime,kernelTime,periodUserTime,periodKernelTime;
        public uint faults,totalProcesses,activeProcesses,terminatedProcesses;
    }
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern bool CreateProcess(string app, StringBuilder command, IntPtr pa, IntPtr ta, bool inherit,
        uint flags, IntPtr env, string cwd, ref STARTUPINFO startup, out PROCESS_INFORMATION info);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern IntPtr CreateJobObject(IntPtr attributes,string name);
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern bool SetInformationJobObject(IntPtr job,int information, ref EXTENDED_LIMITS data,uint size);
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern bool QueryInformationJobObject(IntPtr job,int information, out ACCOUNTING data,uint size,IntPtr returned);
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern bool AssignProcessToJobObject(IntPtr job,IntPtr process);
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern bool TerminateJobObject(IntPtr job,uint code);
    [DllImport("kernel32.dll", SetLastError=true)] static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool TerminateProcess(IntPtr process,uint code);
    [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForSingleObject(IntPtr handle,uint milliseconds);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr process,out uint code);
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(uint access,bool inherit,uint pid);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool SetConsoleCtrlHandler(Handler routine,bool add);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool GenerateConsoleCtrlEvent(uint kind,uint group);
    [DllImport("kernel32.dll", SetLastError=true)] static extern uint GetConsoleProcessList([Out] uint[] ids,uint length);

    static void Require(bool ok,string message) {
        if (!ok) throw new InvalidOperationException(message+" (Win32="+Marshal.GetLastWin32Error()+")");
    }
    static Dictionary<string,object> Read(string path) {
        return Json.Deserialize<Dictionary<string,object>>(File.ReadAllText(path));
    }
    static void Save(string path,object value) {
        string tmp=path+".tmp";
        File.WriteAllText(tmp,Json.Serialize(value),new UTF8Encoding(false));
        File.Move(tmp,path);
    }
    static string Quote(string arg) {
        // Standard CommandLineToArgvW quoting, including trailing backslashes.
        StringBuilder b=new StringBuilder("\""); int slashes=0;
        foreach(char c in arg) {
            if(c=='\\') { slashes++; continue; }
            if(c=='\"') { b.Append('\\',slashes*2+1); b.Append(c); slashes=0; continue; }
            b.Append('\\',slashes); slashes=0; b.Append(c);
        }
        b.Append('\\',slashes*2); b.Append('"'); return b.ToString();
    }
    static uint[] ConsoleMembers() {
        uint[] ids=new uint[128]; uint n=GetConsoleProcessList(ids,(uint)ids.Length);
        Require(n>0 && n<=ids.Length,"Cannot enumerate isolated console");
        return ids.Take((int)n).ToArray();
    }
    static bool JobEmpty(IntPtr job) {
        ACCOUNTING data;
        Require(QueryInformationJobObject(job,1,out data,(uint)Marshal.SizeOf(typeof(ACCOUNTING)),IntPtr.Zero),"Cannot query owned job");
        return data.activeProcesses==0;
    }
    static bool Exited(IntPtr process,uint milliseconds) {
        uint result=WaitForSingleObject(process,milliseconds);
        if(result==0) return true;
        if(result==258) return false;
        Require(false,"Cannot wait on retained process handle");
        return false;
    }
    static int ProcessCode(IntPtr process) {
        uint code;
        Require(GetExitCodeProcess(process,out code),"Cannot read retained process exit code");
        return unchecked((int)code);
    }
    static int Controller(string configPath) {
        Dictionary<string,object> config=Read(configPath);
        string dir=(string)config["directory"];
        IntPtr job=CreateJobObject(IntPtr.Zero,null);
        Require(job!=IntPtr.Zero,"Cannot create owned job");
        PROCESS_INFORMATION info=new PROCESS_INFORMATION(); bool assigned=false;
        Stopwatch elapsed=Stopwatch.StartNew();
        try {
            EXTENDED_LIMITS limits=new EXTENDED_LIMITS(); limits.basic.flags=KILL_ON_CLOSE;
            Require(SetInformationJobObject(job,9,ref limits,(uint)Marshal.SizeOf(typeof(EXTENDED_LIMITS))),"Cannot set job kill-on-close");
            string self=Process.GetCurrentProcess().MainModule.FileName;
            STARTUPINFO startup=new STARTUPINFO(); startup.cb=(uint)Marshal.SizeOf(typeof(STARTUPINFO));
            string command=Quote(self)+" host "+Quote(configPath)+" "+Process.GetCurrentProcess().Id;
            Require(CreateProcess(self,new StringBuilder(command),IntPtr.Zero,IntPtr.Zero,false,
                NEW_CONSOLE|SUSPENDED,IntPtr.Zero,dir,ref startup,out info),"Cannot start isolated console host");
            Require(AssignProcessToJobObject(job,info.process),"Cannot assign console host to owned job");
            assigned=true; Require(ResumeThread(info.thread)!=UInt32.MaxValue,"Cannot release host start gate");
            Require(Exited(info.process,70000),"Console host deadline exceeded");
            int hostCode=ProcessCode(info.process);
            Stopwatch drain=Stopwatch.StartNew();
            while(!JobEmpty(job) && drain.ElapsedMilliseconds<12000) Thread.Sleep(25);
            bool drained=JobEmpty(job);
            Save(Path.Combine(dir,"controller-result.json"),new {
                controllerPid=Process.GetCurrentProcess().Id,hostPid=info.pid,
                hostExitCode=hostCode,jobDrained=drained,totalMs=elapsed.ElapsedMilliseconds,
                cleanupAuthority="retained owned Job Object, no external PID termination"
            });
            Require(drained,"Probe descendants did not drain");
            return hostCode;
        } finally {
            // Terminate only this freshly created retained job (or its own
            // suspended host if assignment failed); never kill by lookup PID.
            if(assigned) TerminateJobObject(job,99);
            else if(info.process!=IntPtr.Zero) TerminateProcess(info.process,99);
            if(info.thread!=IntPtr.Zero) CloseHandle(info.thread);
            if(info.process!=IntPtr.Zero) CloseHandle(info.process);
            CloseHandle(job);
        }
    }
    static int Host(string configPath,int parentPid) {
        Dictionary<string,object> config=Read(configPath);
        string dir=(string)config["directory"], ready=Path.Combine(dir,"fixture-ready.json");
        string cleanup=Path.Combine(dir,"fixture-cleanup.json");
        uint kind=Convert.ToUInt32(config["event"]);
        uint[] initial=ConsoleMembers();
        Require(initial.Length==1 && initial[0]==(uint)Process.GetCurrentProcess().Id,"Host is not alone in a new console");
        Require(!initial.Contains((uint)parentPid),"Controller unexpectedly shares probe console");
        handler=delegate(uint value) {
            if(value==0||value==1) { Interlocked.Increment(ref observed); return true; }
            return false;
        };
        Require(SetConsoleCtrlHandler(null,false),"Cannot reset inherited Ctrl+C ignore");
        Require(SetConsoleCtrlHandler(handler,true),"Cannot observe fixture console events");
        string file=(string)config["file"];
        var arguments=((System.Collections.IEnumerable)config["arguments"]).Cast<object>();
        string targetArguments=config.ContainsKey("rawArguments")?(string)config["rawArguments"]:
            string.Join(" ",arguments.Select(v=>Quote((string)v)).ToArray());
        ProcessStartInfo start=new ProcessStartInfo(file,targetArguments);
        start.WorkingDirectory=(string)config["workingDirectory"];
        start.UseShellExecute=false; start.CreateNoWindow=false;
        start.RedirectStandardInput=true; start.RedirectStandardOutput=true; start.RedirectStandardError=true;
        start.EnvironmentVariables.Remove("FAILFORGE_CLI_CANCEL_PIPE");
        using(Process target=Process.Start(start)) {
            target.StandardInput.Close();
            Thread outThread=new Thread(delegate() { using(FileStream output=File.Create(Path.Combine(dir,"stdout.log"))) target.StandardOutput.BaseStream.CopyTo(output); });
            Thread errThread=new Thread(delegate() { using(FileStream output=File.Create(Path.Combine(dir,"stderr.log"))) target.StandardError.BaseStream.CopyTo(output); });
            outThread.IsBackground=true; errThread.IsBackground=true; outThread.Start(); errThread.Start();
            Save(Path.Combine(dir,"host-ready.json"),new { hostPid=Process.GetCurrentProcess().Id,targetPid=target.Id,isolatedConsole=true,initialConsole=initial });
            Stopwatch wait=Stopwatch.StartNew();
            while(!File.Exists(ready)) {
                Require(!target.HasExited,"Target exited before fixture readiness");
                Require(wait.ElapsedMilliseconds<45000,"Fixture readiness deadline exceeded"); Thread.Sleep(25);
            }
            int fixturePid=Convert.ToInt32(Read(ready)["pid"]);
            // Observation only: cleanup uses the freshly created retained Job.
            IntPtr fixture=OpenProcess(0x00100000|0x1000,false,(uint)fixturePid);
            Require(fixture!=IntPtr.Zero,"Cannot observe ready fixture process");
            try {
                Thread.Sleep(200);
                uint[] members=ConsoleMembers();
                Require(members.Contains((uint)target.Id)&&!members.Contains((uint)parentPid),"Target escaped isolated console");
                Stopwatch signal=Stopwatch.StartNew();
                Require(GenerateConsoleCtrlEvent(kind,0),"Cannot broadcast fixture-only console event");
                Require(target.WaitForExit(20000),"Target cancellation deadline exceeded");
                long targetExitedMs=signal.ElapsedMilliseconds;
                bool aliveAtExit=!Exited(fixture,0), cleanupAtExit=File.Exists(cleanup);
                Require(Exited(fixture,15000),"Fixture cleanup deadline exceeded");
                // Wait for intermediate VM/native processes as well. The
                // controller separately proves the complete owned job drains.
                wait.Restart();
                while(ConsoleMembers().Any(pid=>pid!=(uint)Process.GetCurrentProcess().Id) && wait.ElapsedMilliseconds<10000) Thread.Sleep(25);
                Require(Volatile.Read(ref observed)>0,"Host did not observe real console event");
                Require(outThread.Join(5000)&&errThread.Join(5000),"Capture streams did not drain");
                Save(Path.Combine(dir,"host-result.json"),new {
                    mode=(string)config["name"],eventKind=kind,eventApi="GenerateConsoleCtrlEvent",eventGroup=0,
                    hostObserved=observed,targetPid=target.Id,targetExitCode=target.ExitCode,
                    targetExitAfterSignalMs=targetExitedMs,fixturePid=fixturePid,fixtureAliveAtTargetExit=aliveAtExit,
                    fixtureCleanupAtTargetExit=cleanupAtExit,fixtureCleanupFinal=File.Exists(cleanup),
                    fixtureExitCode=ProcessCode(fixture),allObservedConsoleMembers=members,
                    fixtureCleanup=File.Exists(cleanup)?Read(cleanup):null,
                    finalConsoleMembers=ConsoleMembers()
                });
            } finally { CloseHandle(fixture); }
        }
        GC.KeepAlive(handler);
        return 0;
    }
    static int Fixture(string directory,bool signals) {
        Directory.CreateDirectory(directory);
        int count=0; uint first=99;
        if(signals) {
            handler=delegate(uint kind) {
                if(kind!=0&&kind!=1) return false;
                if(Interlocked.Increment(ref count)==1) {
                    first=kind;
                    Thread cleanup=new Thread(delegate() {
                        Thread.Sleep(2000);
                        Save(Path.Combine(directory,"fixture-cleanup.json"),new { pid=Process.GetCurrentProcess().Id,kind=first,count=count,cleanupDelayMs=2000 });
                        Environment.Exit(first==0?130:143);
                    });
                    cleanup.IsBackground=true; cleanup.Start();
                }
                return true;
            };
            Require(SetConsoleCtrlHandler(null,false),"Cannot reset fixture Ctrl+C ignore");
            Require(SetConsoleCtrlHandler(handler,true),"Cannot install fixture handler");
        }
        Save(Path.Combine(directory,"fixture-ready.json"),new {pid=Process.GetCurrentProcess().Id,signals=signals});
        Thread.Sleep(60000);
        return 77;
    }
    public static int Main(string[] args) {
        try {
            if(args[0]=="controller") return Controller(Path.GetFullPath(args[1]));
            if(args[0]=="host") return Host(Path.GetFullPath(args[1]),Int32.Parse(args[2]));
            if(args[0]=="fixture-signal") return Fixture(Path.GetFullPath(args[1]),true);
            if(args[0]=="fixture-hold") return Fixture(Path.GetFullPath(args[1]),false);
            throw new ArgumentException("Unknown probe mode");
        } catch(Exception error) {
            Console.Error.WriteLine(error.ToString());
            if(args.Length>1 && args[0]=="host") {
                try { Save(Path.Combine(Path.GetDirectoryName(args[1]),"host-error.json"),new { error=error.ToString() }); } catch { }
            }
            return 2;
        }
    }
}
