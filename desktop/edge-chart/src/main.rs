#region Using declarations
using System;
using System.Collections.Generic;
using System.Globalization;
using System.Net.Sockets;
using System.Text;
using NinjaTrader.Cbi;
using NinjaTrader.Core;
using NinjaTrader.Data;
using NinjaTrader.NinjaScript;
#endregion

// EDGE GPU chart feed: add this to ANY chart of ES / NQ / CL / GC / NG (or the micros).
// It requests its own history (about 3.5 months of 1-minute bars and ~10 years of daily bars, joined across contract rolls)
// and streams it, then live updates, to the EDGE GPU chart on 127.0.0.1:7878. The chart's "Days to load" does not matter.
namespace NinjaTrader.NinjaScript.Indicators
{
	public class EdgeFeed : Indicator
	{
		private const int Port = 7878;
		private const int MinuteBars = 150000;
		private const int DailyBars = 2500;

		private BarsRequest reqM;
		private BarsRequest reqD;
		private Bars barsM;
		private Bars barsD;
		private TcpClient client;
		private NetworkStream stream;
		private readonly object gate = new object();
		private System.Threading.Timer timer;
		private string sym;
		private bool linked;
		private int lastIdxM = -1;
		private int lastIdxD = -1;
		private DateTime lastSendM = DateTime.MinValue;
		private DateTime lastSendD = DateTime.MinValue;
		private static readonly DateTime Epoch = new DateTime(1970, 1, 1, 0, 0, 0, DateTimeKind.Utc);

		protected override void OnStateChange()
		{
			if (State == State.SetDefaults)
			{
				Name = "EdgeFeed";
				Description = "Streams futures bars to the EDGE GPU chart";
				Calculate = Calculate.OnBarClose;
				IsOverlay = true;
				DisplayInDataBox = false;
				PaintPriceMarkers = false;
			}
			else if (State == State.DataLoaded)
			{
				sym = Instrument.MasterInstrument.Name;
				EdgeTradeLink.Start();
				EdgeOrderLog.Start();
				linked = true;
				EdgeBarServer.Register(sym, this);
				StartRequests();
				timer = new System.Threading.Timer(Tick, null, 1000, 3000);
			}
			else if (State == State.Terminated)
			{
				if (linked) { linked = false; EdgeTradeLink.Stop(); EdgeOrderLog.Stop(); EdgeBarServer.Unregister(sym, this); }
				if (timer != null) { timer.Dispose(); timer = null; }
				if (reqM != null) { reqM.Update -= OnUpdateM; reqM.Dispose(); reqM = null; }
				if (reqD != null) { reqD.Update -= OnUpdateD; reqD.Dispose(); reqD = null; }
				lock (gate) { CloseLocked(); }
			}
		}

		protected override void OnBarUpdate() { }

		private void StartRequests()
		{
			reqM = new BarsRequest(Instrument, MinuteBars);
			reqM.BarsPeriod = new BarsPeriod { BarsPeriodType = BarsPeriodType.Minute, Value = 1 };
			reqM.TradingHours = Bars.TradingHours;
			reqM.MergePolicy = MergePolicy.MergeNonBackAdjusted;
			reqM.Update += OnUpdateM;
			reqM.Request(new Action<BarsRequest, ErrorCode, string>((r, ec, msg) =>
			{
				if (ec != ErrorCode.NoError) { Print("EdgeFeed 1-minute request failed: " + msg); return; }
				barsM = r.Bars;
				Print("EdgeFeed: got " + barsM.Count + " 1-minute bars for " + sym + ", first " + barsM.GetTime(0) + ", last " + barsM.GetTime(barsM.Count - 1));
			}));

			reqD = new BarsRequest(Instrument, DailyBars);
			reqD.BarsPeriod = new BarsPeriod { BarsPeriodType = BarsPeriodType.Day, Value = 1 };
			reqD.TradingHours = Bars.TradingHours;
			reqD.MergePolicy = MergePolicy.MergeNonBackAdjusted;
			reqD.Update += OnUpdateD;
			reqD.Request(new Action<BarsRequest, ErrorCode, string>((r, ec, msg) =>
			{
				if (ec != ErrorCode.NoError) { Print("EdgeFeed daily request failed: " + msg); return; }
				barsD = r.Bars;
				Print("EdgeFeed: got " + barsD.Count + " daily bars for " + sym + ", first " + barsD.GetTime(0) + ", last " + barsD.GetTime(barsD.Count - 1));
			}));
		}

		// every 3 seconds: if the GPU chart is not connected, try to connect and send the full history
		private void Tick(object state)
		{
			lock (gate)
			{
				if (stream != null || barsM == null) return;
				try
				{
					client = new TcpClient();
					client.NoDelay = true;
					client.Connect("127.0.0.1", Port);
					stream = client.GetStream();
				}
				catch
				{
					CloseLocked();
					return;
				}
				Print("EdgeFeed: connected to the GPU chart, sending history");
				Send(barsM.Count, i => barsM.GetTime(i), i => barsM.GetOpen(i), i => barsM.GetHigh(i), i => barsM.GetLow(i), i => barsM.GetClose(i), 0, barsM.Count - 1, false, true);
				if (barsD != null) Send(barsD.Count, i => barsD.GetTime(i), i => barsD.GetOpen(i), i => barsD.GetHigh(i), i => barsD.GetLow(i), i => barsD.GetClose(i), 0, barsD.Count - 1, true, true);
			}
		}

		private void OnUpdateM(object sender, BarsUpdateEventArgs e)
		{
			if (e.MaxIndex < e.MinIndex) return;
			bool newBar = e.MaxIndex != lastIdxM;
			if (!newBar && (DateTime.Now - lastSendM).TotalMilliseconds < 200) return;
			lastIdxM = e.MaxIndex;
			lastSendM = DateTime.Now;
			BarsSeries bs = e.BarsSeries;
			lock (gate) { Send(bs.Count, i => bs.GetTime(i), i => bs.GetOpen(i), i => bs.GetHigh(i), i => bs.GetLow(i), i => bs.GetClose(i), Math.Max(0, newBar ? e.MaxIndex - 1 : e.MinIndex), e.MaxIndex, false, false); }
		}

		private void OnUpdateD(object sender, BarsUpdateEventArgs e)
		{
			if (e.MaxIndex < e.MinIndex) return;
			bool newBar = e.MaxIndex != lastIdxD;
			if (!newBar && (DateTime.Now - lastSendD).TotalMilliseconds < 500) return;
			lastIdxD = e.MaxIndex;
			lastSendD = DateTime.Now;
			BarsSeries bs = e.BarsSeries;
			lock (gate) { Send(bs.Count, i => bs.GetTime(i), i => bs.GetOpen(i), i => bs.GetHigh(i), i => bs.GetLow(i), i => bs.GetClose(i), Math.Max(0, newBar ? e.MaxIndex - 1 : e.MinIndex), e.MaxIndex, true, false); }
		}

		private void CloseLocked()
		{
			try { if (stream != null) stream.Close(); } catch { }
			try { if (client != null) client.Close(); } catch { }
			stream = null;
			client = null;
		}

		// 1-minute bars as JSON for the EDGE web chart (see EdgeBarServer): ascending [openUtcSec,o,h,l,c]. n = newest n bars (0 = no limit), from/to = unix seconds (0 = unbounded)
		internal string BarsJson(long fromT, long toT, int n)
		{
			lock (gate)
			{
				if (barsM == null || barsM.Count <= 0) return null;
				TimeZoneInfo tz = Globals.GeneralOptions.TimeZoneInfo;
				List<string> rows = new List<string>();
				for (int bar = barsM.Count - 1; bar >= 0; bar--)
				{
					DateTime open = DateTime.SpecifyKind(barsM.GetTime(bar).AddMinutes(-1), DateTimeKind.Unspecified);
					long t = (long)(TimeZoneInfo.ConvertTimeToUtc(open, tz) - Epoch).TotalSeconds;
					if (toT > 0 && t > toT) continue;
					if (fromT > 0 && t < fromT) break;
					rows.Add("[" + t.ToString(CultureInfo.InvariantCulture) + ","
						+ barsM.GetOpen(bar).ToString(CultureInfo.InvariantCulture) + ","
						+ barsM.GetHigh(bar).ToString(CultureInfo.InvariantCulture) + ","
						+ barsM.GetLow(bar).ToString(CultureInfo.InvariantCulture) + ","
						+ barsM.GetClose(bar).ToString(CultureInfo.InvariantCulture) + "]");
					if (n > 0 && rows.Count >= n) break;
				}
				rows.Reverse();
				return "{\"sym\":\"" + sym + "\",\"bars\":[" + string.Join(",", rows.ToArray()) + "]}";
			}
		}

		// call with the gate held
		private void Send(int count, Func<int, DateTime> getTime, Func<int, double> getO, Func<int, double> getH, Func<int, double> getL, Func<int, double> getC, int fromBar, int toBar, bool daily, bool reset)
		{
			if (stream == null || count <= 0 || toBar < fromBar || toBar >= count) return;
			try
			{
				TimeZoneInfo tz = Globals.GeneralOptions.TimeZoneInfo;
				StringBuilder sb = new StringBuilder(1024);
				sb.Append("{\"sym\":\"").Append(sym).Append("\",");
				if (daily) sb.Append("\"tf\":\"D\",");
				if (reset) sb.Append("\"reset\":true,");
				sb.Append("\"bars\":[");
				bool first = true;
				for (int bar = fromBar; bar <= toBar; bar++)
				{
					// NinjaTrader stamps a bar with its CLOSE time; the chart wants the OPEN time in UTC
					DateTime open = DateTime.SpecifyKind(getTime(bar).AddMinutes(-1), DateTimeKind.Unspecified);
					DateTime utc = TimeZoneInfo.ConvertTimeToUtc(open, tz);
					long t = (long)(utc - Epoch).TotalSeconds;
					if (!first) sb.Append(',');
					first = false;
					sb.Append('[').Append(t.ToString(CultureInfo.InvariantCulture)).Append(',')
					  .Append(getO(bar).ToString(CultureInfo.InvariantCulture)).Append(',')
					  .Append(getH(bar).ToString(CultureInfo.InvariantCulture)).Append(',')
					  .Append(getL(bar).ToString(CultureInfo.InvariantCulture)).Append(',')
					  .Append(getC(bar).ToString(CultureInfo.InvariantCulture)).Append(']');
				}
				sb.Append("]}\n");
				byte[] data = Encoding.UTF8.GetBytes(sb.ToString());
				stream.Write(data, 0, data.Length);
			}
			catch
			{
				CloseLocked();
			}
		}
	}

	// Local HTTP endpoint for the EDGE web chart: GET http://127.0.0.1:7881/bars?sym=NQ&n=1000 (or &from=unixSec&to=unixSec) returns the
	// 1-minute futures bars this indicator already holds, so the web chart can draw real futures candles (exact fill prices) instead of the OANDA CFD.
	// One shared listener no matter how many EdgeFeed charts are open; each chart registers under its own symbol.
	internal static class EdgeBarServer
	{
		private const int HttpPort = 7881;
		private static readonly object lk = new object();
		private static readonly Dictionary<string, EdgeFeed> feeds = new Dictionary<string, EdgeFeed>();
		private static int users;
		private static TcpListener listener;

		public static void Register(string sym, EdgeFeed f)
		{
			lock (lk)
			{
				feeds[sym] = f; users++; try { EdgeCmd.Key(); } catch { }
				if (listener != null) return;
				try
				{
					listener = new TcpListener(System.Net.IPAddress.Loopback, HttpPort);
					listener.Start();
				}
				catch { listener = null; return; }
				TcpListener l = listener;
				System.Threading.Thread th = new System.Threading.Thread(delegate() { Loop(l); });
				th.IsBackground = true;
				th.Start();
			}
		}

		public static void Unregister(string sym, EdgeFeed f)
		{
			lock (lk)
			{
				EdgeFeed cur;
				if (feeds.TryGetValue(sym, out cur) && cur == f) feeds.Remove(sym);
				users--;
				if (users <= 0)
				{
					users = 0;
					try { if (listener != null) listener.Stop(); } catch { }
					listener = null;
				}
			}
		}

		private static void Loop(TcpListener l)
		{
			while (true)
			{
				TcpClient c;
				try { c = l.AcceptTcpClient(); } catch { return; }
				System.Threading.ThreadPool.QueueUserWorkItem(delegate(object o) { Handle((TcpClient)o); }, c);
			}
		}

		private static EdgeFeed Find(string want)
		{
			string[] names;
			switch (want)
			{
				case "NQ": names = new string[] { "NQ", "MNQ" }; break;
				case "ES": names = new string[] { "ES", "MES" }; break;
				case "CL": names = new string[] { "CL", "MCL" }; break;
				case "GC": names = new string[] { "GC", "MGC" }; break;
				case "NG": names = new string[] { "NG", "MNG" }; break;
				default: names = new string[] { want }; break;
			}
			lock (lk)
			{
				foreach (string nm in names)
				{
					EdgeFeed f;
					if (feeds.TryGetValue(nm, out f)) return f;
				}
			}
			return null;
		}

		private static void Handle(TcpClient c)
		{
			try
			{
				c.ReceiveTimeout = 2000; c.SendTimeout = 8000;
				NetworkStream s = c.GetStream();
				byte[] buf = new byte[8192]; int n = 0;
				while (n < buf.Length)
				{
					int r = s.Read(buf, n, buf.Length - n);
					if (r <= 0) break;
					n += r;
					if (Encoding.ASCII.GetString(buf, 0, n).IndexOf("\r\n\r\n") >= 0) break;
				}
				string req = Encoding.ASCII.GetString(buf, 0, n);
				string[] parts = req.Split('\n')[0].Trim().Split(' ');
				string method = parts.Length > 0 ? parts[0] : "";
				string target = parts.Length > 1 ? parts[1] : "/";
				int code = 200; string body = "{}";
				if (method == "OPTIONS") { code = 204; body = ""; }
				else if (target.StartsWith("/bars"))
				{
					string sym = "", qs = target.IndexOf('?') >= 0 ? target.Substring(target.IndexOf('?') + 1) : "";
					long fromT = 0, toT = 0; int cnt = 0;
					foreach (string kv in qs.Split('&'))
					{
						string[] p = kv.Split('=');
						if (p.Length != 2) continue;
						if (p[0] == "sym") sym = p[1].ToUpperInvariant();
						else if (p[0] == "from") long.TryParse(p[1], NumberStyles.Integer, CultureInfo.InvariantCulture, out fromT);
						else if (p[0] == "to") long.TryParse(p[1], NumberStyles.Integer, CultureInfo.InvariantCulture, out toT);
						else if (p[0] == "n") int.TryParse(p[1], NumberStyles.Integer, CultureInfo.InvariantCulture, out cnt);
					}
					EdgeFeed f = Find(sym);
					string json = f == null ? null : f.BarsJson(fromT, toT, cnt > 0 ? Math.Min(cnt, 150000) : 0);
					if (json == null) code = 404; else body = json;
				}
				else if (target.StartsWith("/orders"))
				{
					string osym = "", oqs = target.IndexOf('?') >= 0 ? target.Substring(target.IndexOf('?') + 1) : "";
					long ofrom = 0, oto = 0;
					foreach (string kv in oqs.Split('&'))
					{
						string[] p = kv.Split('=');
						if (p.Length != 2) continue;
						if (p[0] == "sym") osym = p[1].ToUpperInvariant();
						else if (p[0] == "from") long.TryParse(p[1], NumberStyles.Integer, CultureInfo.InvariantCulture, out ofrom);
						else if (p[0] == "to") long.TryParse(p[1], NumberStyles.Integer, CultureInfo.InvariantCulture, out oto);
					}
					body = EdgeOrderLog.Json(osym, ofrom, oto);
				}
				else if (target.StartsWith("/move") || target.StartsWith("/close"))
				{
					string cq = target.IndexOf('?') >= 0 ? target.Substring(target.IndexOf('?') + 1) : "";
					string perr;
					if (method != "POST") { code = 405; perr = "POST only"; }
					else perr = EdgeCmd.Auth(req, cq);
					if (perr != null) { if (code == 200) code = 403; body = EdgeCmd.Err(perr); }
					else body = EdgeCmd.Run(target.StartsWith("/move"), cq);
				}
				else code = 404;
				byte[] data = Encoding.UTF8.GetBytes(body);
				string head = "HTTP/1.1 " + code + (code == 200 ? " OK" : (code == 204 ? " No Content" : (code == 403 ? " Forbidden" : (code == 405 ? " Method Not Allowed" : " Not Found"))))
					+ "\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Private-Network: true\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *"
					+ "\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: " + data.Length + "\r\n\r\n";
				byte[] h = Encoding.ASCII.GetBytes(head);
				s.Write(h, 0, h.Length);
				if (data.Length > 0) s.Write(data, 0, data.Length);
			}
			catch { }
			finally { try { c.Close(); } catch { } }
		}
	}

	// Commands the charts send to move a stop / target or close a trade. Guarded three ways: POST only, the request must come from this PC
	// (Host check) and from an allowed page (Origin) or carry the secret key, and on every account.
	// Files live in %APPDATA%\\EdgeFeed: key.txt (secret key, made on first run), origins.txt (extra allowed pages, one per line).
	internal static class EdgeCmd
	{
		private static string key;
		private static readonly object klk = new object();

		public static string Dir() { return System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "EdgeFeed"); }

		public static string Key()
		{
			lock (klk)
			{
				if (key != null) return key;
				try
				{
					string f = System.IO.Path.Combine(Dir(), "key.txt");
					System.IO.Directory.CreateDirectory(Dir());
					if (System.IO.File.Exists(f)) key = System.IO.File.ReadAllText(f).Trim();
					if (string.IsNullOrEmpty(key))
					{
						byte[] b = new byte[16];
						using (System.Security.Cryptography.RNGCryptoServiceProvider g = new System.Security.Cryptography.RNGCryptoServiceProvider()) g.GetBytes(b);
						StringBuilder sb = new StringBuilder();
						foreach (byte x in b) sb.Append(x.ToString("x2"));
						key = sb.ToString();
						System.IO.File.WriteAllText(f, key);
					}
				}
				catch { key = ""; }
				return key;
			}
		}

		public static bool AllowAcct(string name)
		{
			return name != null; // every account (live, prop, Sim) can be moved / closed from the chart
		}

		private static string Hdr(string req, string name)
		{
			string[] lines = req.Split('\n');
			for (int i = 1; i < lines.Length; i++)
			{
				string ln = lines[i].Trim();
				if (ln.Length > name.Length && ln[name.Length] == ':' && ln.Substring(0, name.Length).Equals(name, StringComparison.OrdinalIgnoreCase)) return ln.Substring(name.Length + 1).Trim();
			}
			return "";
		}

		private static bool OriginOk(string o)
		{
			if (o == "https://mulamone.github.io" || o.StartsWith("http://localhost:") || o.StartsWith("http://127.0.0.1:")) return true;
			try
			{
				string f = System.IO.Path.Combine(Dir(), "origins.txt");
				if (System.IO.File.Exists(f)) foreach (string ln in System.IO.File.ReadAllLines(f)) if (ln.Trim().Length > 0 && ln.Trim().TrimEnd('/') == o) return true;
			}
			catch { }
			return false;
		}

		// null = allowed, otherwise the reason
		public static string Auth(string req, string qs)
		{
			string host = Hdr(req, "host"), origin = Hdr(req, "origin"), k = Hdr(req, "x-edge-key");
			if (host != "127.0.0.1:7881" && host != "localhost:7881") return "blocked (bad host)";
			if (k.Length == 0) k = Q(qs, "k");
			string mk = Key();
			if (k.Length > 0 && mk.Length > 0 && k == mk) return null;
			if (origin.Length > 0 && OriginOk(origin)) return null;
			return "blocked: " + (origin.Length > 0 ? origin : "this app") + " is not allowed. Use the key in " + System.IO.Path.Combine(Dir(), "key.txt") + " or add the page to origins.txt";
		}

		public static string Q(string qs, string name)
		{
			foreach (string kv in qs.Split('&'))
			{
				int i = kv.IndexOf('=');
				if (i <= 0 || kv.Substring(0, i) != name) continue;
				try { return Uri.UnescapeDataString(kv.Substring(i + 1).Replace('+', ' ')); } catch { return ""; }
			}
			return "";
		}

		private static string Esc(string m) { return (m ?? "").Replace("\\", "/").Replace("\"", "'").Replace("\r", " ").Replace("\n", " "); }
		public static string Err(string m) { return "{\"ok\":false,\"err\":\"" + Esc(m) + "\"}"; }
		public static string Ok(string m) { return "{\"ok\":true,\"msg\":\"" + Esc(m) + "\"}"; }

		public static string Run(bool move, string qs)
		{
			if (move)
			{
				double px;
				if (!double.TryParse(Q(qs, "px"), NumberStyles.Float, CultureInfo.InvariantCulture, out px)) return Err("bad price");
				return EdgeOrderLog.Move(Q(qs, "id"), px);
			}
			return EdgeOrderLog.Close(Q(qs, "acct"), Q(qs, "sym").ToUpperInvariant());
		}
	}

	// Records the life of every stop / target order on every account (placed, moved, filled, cancelled) so the EDGE web chart
	// can draw the stop loss and target each trade really had. In memory only: it starts empty each time NinjaTrader starts.
	internal static class EdgeOrderLog
	{
		private class Ev { public long T; public string Id, Kind, Act, St, Master; public double Px; public int Qty; }
		private static readonly object lk = new object();
		private static readonly HashSet<string> hooked = new HashSet<string>();
		private static readonly Dictionary<string, double> lastPx = new Dictionary<string, double>();
		private static readonly List<Ev> evs = new List<Ev>();
		private static readonly Dictionary<Order, string> ids = new Dictionary<Order, string>();
		private static int nextId;
		private static readonly DateTime Epoch = new DateTime(1970, 1, 1, 0, 0, 0, DateTimeKind.Utc);
		private static System.Threading.Timer timer;
		private static int users;
		private static void Log(string m) { try { NinjaTrader.Code.Output.Process("EdgeOrderLog " + DateTime.Now.ToString("HH:mm:ss") + " " + m, PrintTo.OutputTab1); } catch { } }

		public static void Start()
		{
			lock (lk)
			{
				users++;
				if (timer == null) timer = new System.Threading.Timer(Tick, null, 0, 3000);
			}
		}

		public static void Stop()
		{
			bool last = false;
			lock (lk)
			{
				users--;
				if (users <= 0)
				{
					users = 0; last = true;
					if (timer != null) { timer.Dispose(); timer = null; }
					hooked.Clear();
				}
			}
			if (last)
			{
				try { lock (Account.All) { foreach (Account a in Account.All) a.OrderUpdate -= OnOrder; } } catch { }
			}
		}

		private static void Tick(object state)
		{
			try
			{
				List<Account> fresh = new List<Account>();
				lock (Account.All)
				{
					foreach (Account a in Account.All)
					{
						lock (lk) { if (hooked.Contains(a.Name)) continue; hooked.Add(a.Name); }
						a.OrderUpdate += OnOrder; fresh.Add(a); Log("hooked account " + a.Name);
					}
				}
				foreach (Account a in fresh)
				{
					try
					{
						lock (a.Orders)
						{
							foreach (Order o in a.Orders)
								if (o.OrderState == OrderState.Working || o.OrderState == OrderState.Accepted || o.OrderState == OrderState.TriggerPending) Record(o, o.OrderState, o.Time);
						}
					}
					catch { }
				}
			}
			catch { }
		}

		private static void OnOrder(object sender, OrderEventArgs e)
		{
			try { if (e != null && e.Order != null) Record(e.Order, e.OrderState, e.Order.Time); } catch { }
		}

		private static void Record(Order o, OrderState st, DateTime time)
		{
			if (o == null || o.Instrument == null || o.Account == null) return;
			string kind; double px;
			// Sim101 holds stops as "simulated stops" (state TriggerPending) whose OrderType can read as Limit, so go by the prices: a stop has a StopPrice, a target is a plain limit
			string nm = o.Name == null ? "" : o.Name;
			if (o.OrderType == OrderType.MIT) { kind = "T"; px = o.StopPrice > 0 ? o.StopPrice : o.LimitPrice; }
			else if (nm.StartsWith("Target") && o.LimitPrice > 0) { kind = "T"; px = o.LimitPrice; } // ATM brackets are named Stop1 / Target1 ...
			else if (nm.StartsWith("Stop") && o.StopPrice > 0) { kind = "S"; px = o.StopPrice; }
			else if (o.StopPrice > 0) { kind = "S"; px = o.StopPrice; }
			else if (o.LimitPrice > 0) { kind = "T"; px = o.LimitPrice; }
			else return;
			string code;
			if (st == OrderState.Working || st == OrderState.Accepted || st == OrderState.TriggerPending) code = "W";
			else if (st == OrderState.Filled) code = "F";
			else if (st == OrderState.Cancelled || st == OrderState.Rejected) code = "C";
			else return;
			bool buy = o.OrderAction == OrderAction.Buy || o.OrderAction == OrderAction.BuyToCover;
			long t = ToMs(time);
			lock (lk)
			{
				// the order's own object identifies it (its OrderId can change over its life)
				string id;
				if (!ids.TryGetValue(o, out id)) { id = o.Account.Name.Replace("\"", "").Replace("\\", "") + "|" + (++nextId).ToString(CultureInfo.InvariantCulture); ids[o] = id; }
				double prev;
				if (code == "W")
				{
					if (lastPx.TryGetValue(id, out prev) && Math.Abs(prev - px) < 1e-9) return; // unchanged, nothing new to record
					lastPx[id] = px;
				}
				else { lastPx.Remove(id); ids.Remove(o); }
				Ev ev = new Ev();
				ev.T = t; ev.Id = id; ev.Kind = kind; ev.Act = buy ? "B" : "S"; ev.St = code; ev.Px = px; ev.Qty = o.Quantity; ev.Master = o.Instrument.MasterInstrument.Name;
				evs.Add(ev);
				Log((kind == "S" ? "stop " : "target ") + code + " " + o.Account.Name + " " + o.Instrument.FullName + " " + (buy ? "Buy" : "Sell") + " @ " + px.ToString("0.#####", CultureInfo.InvariantCulture));
				if (evs.Count > 20000) evs.RemoveRange(0, 2000);
			}
		}

		// same reading of an order as Record(): a stop (S) or a target (T) and its price
		private static bool Classify(Order o, out string kind, out double px)
		{
			string nm = o.Name == null ? "" : o.Name;
			kind = ""; px = 0;
			if (o.OrderType == OrderType.MIT) { kind = "T"; px = o.StopPrice > 0 ? o.StopPrice : o.LimitPrice; }
			else if (nm.StartsWith("Target") && o.LimitPrice > 0) { kind = "T"; px = o.LimitPrice; }
			else if (nm.StartsWith("Stop") && o.StopPrice > 0) { kind = "S"; px = o.StopPrice; }
			else if (o.StopPrice > 0) { kind = "S"; px = o.StopPrice; }
			else if (o.LimitPrice > 0) { kind = "T"; px = o.LimitPrice; }
			else return false;
			return true;
		}

		// Move one working stop / target to a new price (the order's id is the one /orders hands out). Uses StopPriceChanged / LimitPriceChanged + Account.Change,
		// never cancel + resubmit (that would break the ATM). NinjaTrader applies it asynchronously; the charts redraw from the order's real state.
		public static string Move(string id, double px)
		{
			Order o = null;
			lock (lk) { foreach (KeyValuePair<Order, string> kv in ids) { if (kv.Value == id) { o = kv.Key; break; } } }
			if (o == null) return EdgeCmd.Err("that order is no longer working");
			try
			{
				string an = o.Account.Name;
				if (!EdgeCmd.AllowAcct(an)) return EdgeCmd.Err("moving orders from the chart is off for account " + an + " (create live_ok.txt in " + EdgeCmd.Dir() + " to turn it on)");
				OrderState st = o.OrderState;
				if (st != OrderState.Working && st != OrderState.Accepted && st != OrderState.TriggerPending) return EdgeCmd.Err("order is " + st);
				string kind; double cur;
				if (!Classify(o, out kind, out cur)) return EdgeCmd.Err("not a stop or target");
				double r = o.Instrument.MasterInstrument.RoundToTickSize(px);
				if (!(r > 0)) return EdgeCmd.Err("bad price");
				if (Math.Abs(r - cur) < o.Instrument.MasterInstrument.TickSize / 2) return EdgeCmd.Ok("unchanged");
				if (o.OrderType == OrderType.MIT) o.StopPriceChanged = r;
				else if (kind == "S")
				{
					if (o.OrderType == OrderType.StopLimit) o.LimitPriceChanged = r + (o.LimitPrice - o.StopPrice);
					o.StopPriceChanged = r;
				}
				else o.LimitPriceChanged = r;
				Order oo = o; double want = r;
				o.Account.Change(new Order[] { o });
				Log("MOVE " + (kind == "S" ? "stop " : "target ") + an + " " + o.Instrument.FullName + " " + cur.ToString("0.#####", CultureInfo.InvariantCulture) + " -> " + r.ToString("0.#####", CultureInfo.InvariantCulture));
				// a couple of seconds later, say in the Output tab whether NinjaTrader really took it
				System.Threading.Timer chk = null;
				chk = new System.Threading.Timer(delegate(object x)
				{
					try { string k2; double now; if (Classify(oo, out k2, out now)) Log("MOVE check: " + (Math.Abs(now - want) < 1e-9 ? "applied" : "NOT applied (still " + now.ToString("0.#####", CultureInfo.InvariantCulture) + ", state " + oo.OrderState + ")")); } catch { }
					try { if (chk != null) chk.Dispose(); } catch { }
				}, null, 2500, System.Threading.Timeout.Infinite);
				return "{\"ok\":true,\"px\":" + r.ToString("0.#####", CultureInfo.InvariantCulture) + "}";
			}
			catch (Exception ex) { Log("MOVE ERROR " + ex.Message); return EdgeCmd.Err("NinjaTrader refused: " + ex.Message); }
		}

		// Close the trade like the X on NinjaTrader's own chart: market-close the position and cancel its stop and target (Account.Flatten on that instrument).
		public static string Close(string acct, string want)
		{
			try
			{
				if (!EdgeCmd.AllowAcct(acct)) return EdgeCmd.Err("closing from the chart is off for account " + acct + " (create live_ok.txt in " + EdgeCmd.Dir() + " to turn it on)");
				Account a = null;
				lock (Account.All) { foreach (Account x in Account.All) { if (x.Name == acct) { a = x; break; } } }
				if (a == null) return EdgeCmd.Err("account not found");
				string[] names = Names(want);
				List<Instrument> ins = new List<Instrument>();
				lock (a.Positions)
				{
					foreach (Position p in a.Positions)
					{
						if (p == null || p.Instrument == null || p.MarketPosition == MarketPosition.Flat || p.Quantity <= 0) continue;
						if (Array.IndexOf(names, p.Instrument.MasterInstrument.Name) < 0) continue;
						ins.Add(p.Instrument);
					}
				}
				if (ins.Count == 0) return EdgeCmd.Err("no open position to close");
				a.Flatten(ins);
				Log("CLOSE " + acct + " " + ins[0].FullName);
				return EdgeCmd.Ok("closing");
			}
			catch (Exception ex) { Log("CLOSE ERROR " + ex.Message); return EdgeCmd.Err("NinjaTrader refused: " + ex.Message); }
		}

		private static long ToMs(DateTime local)
		{
			try { return (long)(TimeZoneInfo.ConvertTimeToUtc(DateTime.SpecifyKind(local, DateTimeKind.Unspecified), Globals.GeneralOptions.TimeZoneInfo) - Epoch).TotalMilliseconds; }
			catch { return (long)(DateTime.UtcNow - Epoch).TotalMilliseconds; }
		}

		// the stop / target orders that were working during one trade, as a JSON fragment  ,"ord":[[timeMs,"S"|"T",price,"W"|"F"|"C",orderNo],...]  (empty string when there are none).
		// Sent with the closed trade so EDGE can save it. Capped so the whole trade line stays small.
		public static string ForTrade(string acct, string master, DateTime entLocal, DateTime exLocal, bool isLong)
		{
			try
			{
				long a = ToMs(entLocal) - 2000, b = ToMs(exLocal) + 3000;
				string pre = acct + "|", act = isLong ? "S" : "B";
				Dictionary<string, int> idx = new Dictionary<string, int>();
				List<string> rows = new List<string>();
				lock (lk)
				{
					foreach (Ev ev in evs)
					{
						if (ev.T < a || ev.T > b) continue;
						if (ev.Master != master || ev.Act != act || !ev.Id.StartsWith(pre)) continue;
						int n;
						if (!idx.TryGetValue(ev.Id, out n)) { n = idx.Count + 1; idx[ev.Id] = n; }
						if (rows.Count >= 40) break;
						rows.Add("[" + ev.T.ToString(CultureInfo.InvariantCulture) + ",\"" + ev.Kind + "\"," + ev.Px.ToString("0.#####", CultureInfo.InvariantCulture) + ",\"" + ev.St + "\"," + n.ToString(CultureInfo.InvariantCulture) + "]");
					}
				}
				return rows.Count == 0 ? "" : ",\"ord\":[" + string.Join(",", rows.ToArray()) + "]";
			}
			catch { return ""; }
		}

		private static string[] Names(string want)
		{
			switch (want)
			{
				case "NQ": return new string[] { "NQ", "MNQ" };
				case "ES": return new string[] { "ES", "MES" };
				case "CL": return new string[] { "CL", "MCL" };
				case "GC": return new string[] { "GC", "MGC" };
				case "NG": return new string[] { "NG", "MNG" };
				default: return new string[] { want };
			}
		}

		// [timeMs,"id","S"|"T","B"|"S",price,qty,"W"|"F"|"C"], oldest first. from/to are unix seconds (0 = unbounded)
		public static string Json(string want, long fromT, long toT)
		{
			string[] names = Names(want);
			List<string> rows = new List<string>();
			lock (lk)
			{
				foreach (Ev ev in evs)
				{
					long sec = ev.T / 1000;
					if (toT > 0 && sec > toT) continue;
					if (fromT > 0 && sec < fromT) continue;
					if (Array.IndexOf(names, ev.Master) < 0) continue;
					rows.Add("[" + ev.T.ToString(CultureInfo.InvariantCulture) + ",\"" + ev.Id + "\",\"" + ev.Kind + "\",\"" + ev.Act + "\","
						+ ev.Px.ToString("0.#####", CultureInfo.InvariantCulture) + "," + ev.Qty.ToString(CultureInfo.InvariantCulture) + ",\"" + ev.St + "\"]");
				}
			}
			return "{\"sym\":\"" + want + "\",\"orders\":[" + string.Join(",", rows.ToArray()) + "],\"pos\":[" + Positions(names) + "]}";
		}

		// open positions on this symbol, so a chart can show what each stop / target is worth:  ["account",+1 long|-1 short,qty,averagePrice,dollarsPerPoint]
		private static string Positions(string[] names)
		{
			List<string> rows = new List<string>();
			try
			{
				lock (Account.All)
				{
					foreach (Account a in Account.All)
					{
						try
						{
							lock (a.Positions)
							{
								foreach (Position p in a.Positions)
								{
									if (p == null || p.Instrument == null || p.MarketPosition == MarketPosition.Flat || p.Quantity <= 0) continue;
									if (Array.IndexOf(names, p.Instrument.MasterInstrument.Name) < 0) continue;
									rows.Add("[\"" + a.Name.Replace("\"", "").Replace("\\", "") + "\"," + (p.MarketPosition == MarketPosition.Long ? "1" : "-1") + "," + p.Quantity.ToString(CultureInfo.InvariantCulture) + ","
										+ p.AveragePrice.ToString("0.#####", CultureInfo.InvariantCulture) + "," + p.Instrument.MasterInstrument.PointValue.ToString("0.#####", CultureInfo.InvariantCulture) + "]");
								}
							}
						}
						catch { }
					}
				}
			}
			catch { }
			return string.Join(",", rows.ToArray());
		}
	}

	// Auto-import: watches every account's executions, pairs them into round-trip trades (flat -> flat) and sends each
	// closed trade to the EDGE desktop app on 127.0.0.1:7880. One shared link no matter how many EdgeFeed charts are open.
	// Trades are queued while EDGE is not running and sent when it connects; EDGE decides whether auto-import is on.
	internal static class EdgeTradeLink
	{
		private const int TradePort = 7880;
		private static void Log(string m) { string line = "EdgeTradeLink " + DateTime.Now.ToString("HH:mm:ss") + " " + m; try { NinjaTrader.Code.Output.Process(line, PrintTo.OutputTab1); } catch { } try { string lf = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments), "NinjaTrader 8", "log", "EdgeTradeLink.txt"); if (System.IO.File.Exists(lf) && new System.IO.FileInfo(lf).Length > 2000000) System.IO.File.Delete(lf); } catch { } try { System.IO.File.AppendAllText(System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments), "NinjaTrader 8", "log", "EdgeTradeLink.txt"), line + "\r\n"); } catch { } }
		private static readonly object lk = new object();
		private static int users;
		private static System.Threading.Timer timer;
		private static TcpClient client;
		private static NetworkStream stream;
		private static readonly List<string> hooked = new List<string>();
		private static readonly HashSet<string> seen = new HashSet<string>();
		private static readonly Dictionary<string, Pos> pos = new Dictionary<string, Pos>();
		private static readonly Queue<string> pending = new Queue<string>();
		private static DateTime emitSince = DateTime.MinValue; // while replaying history: only trades that closed after this are sent

		private class Pos
		{
			public double Net;          // signed contracts
			public int Dir;             // +1 long, -1 short
			public double EntQty, EntCost, ExQty, ExCost;
			public DateTime EntTime, ExTime;
			public string FirstId, LastId;
			public Account Acct; public Instrument Inst;
			public DateTime LastExec = DateTime.MinValue, MismatchSince = DateTime.MinValue;
		}

		public static void Start()
		{
			lock (lk)
			{
				users++; Log("Start, users=" + users);
				if (users == 1) timer = new System.Threading.Timer(Tick, null, 500, 2000);
			}
		}

		public static void Stop()
		{
			bool last = false;
			lock (lk)
			{
				users--;
				if (users <= 0)
				{
					users = 0; last = true; Log("Stop: last user gone, unhooking accounts");
					if (timer != null) { timer.Dispose(); timer = null; }
					hooked.Clear();
					try { if (stream != null) stream.Close(); } catch { }
					try { if (client != null) client.Close(); } catch { }
					stream = null; client = null;
				}
			}
			if (last)
			{
				try { lock (Account.All) { foreach (Account a in Account.All) a.ExecutionUpdate -= OnExec; } } catch { }
			}
		}

		private static void Tick(object state)
		{
			List<Execution> past = null;
			bool replayed = false;
			try { past = HookAccounts(); } catch (Exception hx) { Log("HookAccounts ERROR " + hx.Message); }
			lock (lk)
			{
				if (past != null && past.Count > 0)
				{
					emitSince = DateTime.Now.Date; // replay everything (so open positions keep their true entry) but only send today's closed trades
					Log("replaying " + past.Count + " historical executions"); DateTime cutoff = DateTime.Now.Date.AddHours(-6); int skipped = 0; foreach (Execution ex in past) { if (ex.Time < cutoff) { skipped++; continue; } Process(ex); } Log("skipped " + skipped + " executions older than " + cutoff); replayed = true;
					emitSince = DateTime.MinValue;
				}
				}
			Reconcile(replayed);
			lock (lk)
			{
				if (stream == null) { try { client = new TcpClient();
						client.NoDelay = true;
						client.Connect("127.0.0.1", TradePort);
						stream = client.GetStream();
						stream.WriteTimeout = 1000; Log("connected to EDGE on " + TradePort + ", pending=" + pending.Count);
					}
					catch { CloseLocked(); return; }
				}
				WriteLine("{\"hb\":1}");
				while (stream != null && pending.Count > 0)
				{
					if (!WriteLine(pending.Peek())) break;
					pending.Dequeue();
				}
			}
		}

		// history can be incomplete (fills NinjaTrader no longer lists), so make the tracker agree with the account's real position.
		// Must be called WITHOUT lk held: the Positions lock is never taken while holding lk (avoids a deadlock with NinjaTrader's adapter thread).
		private static void Reconcile(bool force)
		{
			List<Pos> snap;
			lock (lk) { snap = new List<Pos>(pos.Values); }
			foreach (Pos p in snap)
			{
				if (p.Acct == null || p.Inst == null) continue;
				double actual = 0, avg = 0;
				try
				{
					lock (p.Acct.Positions)
					{
						foreach (Position q in p.Acct.Positions)
						{
							if (q.Instrument != null && q.Instrument.FullName == p.Inst.FullName)
							{
								actual = q.MarketPosition == MarketPosition.Long ? q.Quantity : (q.MarketPosition == MarketPosition.Short ? -q.Quantity : 0);
								avg = q.AveragePrice;
								break;
							}
						}
					}
				}
				catch { continue; }
				lock (lk)
				{
					if (Math.Abs(actual - p.Net) < 1e-9) { p.MismatchSince = DateTime.MinValue; continue; }
					if (!force)
					{
						// live check: ignore the brief gap between an execution and NinjaTrader updating the position
						DateTime now = DateTime.UtcNow;
						if ((now - p.LastExec).TotalSeconds < 5) { p.MismatchSince = DateTime.MinValue; continue; }
						if (p.MismatchSince == DateTime.MinValue) { p.MismatchSince = now; continue; }
						if ((now - p.MismatchSince).TotalSeconds < 4) continue;
					}
					p.MismatchSince = DateTime.MinValue;
					Log("RECONCILE " + p.Acct.Name + "|" + p.Inst.FullName + ": tracker net=" + p.Net + " but account position=" + actual + ", resetting tracker");
					if (Math.Abs(actual) < 1e-9) { p.Net = 0; p.EntQty = p.EntCost = p.ExQty = p.ExCost = 0; }
					else { p.Dir = actual > 0 ? 1 : -1; p.Net = actual; p.EntQty = Math.Abs(actual); p.EntCost = Math.Abs(actual) * avg; p.ExQty = p.ExCost = 0; }
				}
			}
		}

		private static void CloseLocked()
		{
			try { if (stream != null) stream.Close(); } catch { }
			try { if (client != null) client.Close(); } catch { }
			stream = null; client = null;
		}

		private static bool WriteLine(string line)
		{
			if (stream == null) return false;
			try
			{
				byte[] d = Encoding.UTF8.GetBytes(line + "\n");
				stream.Write(d, 0, d.Length);
				return true;
			}
			catch { CloseLocked(); return false; }
		}

		// subscribe to accounts as they appear and return their execution history (oldest first). Runs WITHOUT the link lock
		// so it can never deadlock with NinjaTrader's own threads.
		private static List<Execution> HookAccounts()
		{
			List<Account> fresh = new List<Account>();
			lock (Account.All)
			{
				foreach (Account a in Account.All)
				{
					lock (lk) { if (hooked.Contains(a.Name)) continue; hooked.Add(a.Name); }
					a.ExecutionUpdate += OnExec; fresh.Add(a); Log("hooked account " + a.Name);
				}
			}
			List<Execution> past = new List<Execution>();
			foreach (Account a in fresh)
			{
				try { lock (a.Executions) { int n0 = past.Count; foreach (Execution ex in a.Executions) past.Add(ex); Log("account " + a.Name + " history executions: " + (past.Count - n0)); } } catch (Exception ax) { Log("history read ERROR " + a.Name + " " + ax.Message); }
			}
			past.Sort((x, y) => x.Time.CompareTo(y.Time));
			return past;
		}

		private static void OnExec(object sender, ExecutionEventArgs e)
		{
			if (e == null || e.Execution == null) return;
			if (e.Operation == Operation.Remove) { Log("ignoring REMOVED execution " + e.Execution.ExecutionId); return; } try { Log("LIVE exec " + e.Execution.Account.Name + " " + e.Execution.Instrument.FullName + " qty=" + e.Execution.Quantity + " px=" + e.Execution.Price + " id=" + e.Execution.ExecutionId); } catch { } lock (lk) { Process(e.Execution); }
		}

		// call with the lock held
		private static void Process(Execution ex)
		{
			try
			{
				if (ex.Instrument == null || ex.Account == null || ex.Quantity <= 0) return;
				string id = ex.ExecutionId;
				if (!string.IsNullOrEmpty(id)) { if (seen.Contains(id)) { Log("skip already-seen exec " + id); return; } seen.Add(id); }
				string an = ex.Account.Name;

				int sgn;
				if (ex.Order != null) sgn = (ex.Order.OrderAction == OrderAction.Buy || ex.Order.OrderAction == OrderAction.BuyToCover) ? 1 : -1;
				else sgn = (ex.MarketPosition == MarketPosition.Long) == ex.IsEntry ? 1 : -1;

				string key = an + "|" + ex.Instrument.FullName;
				Pos p;
				if (!pos.TryGetValue(key, out p)) { p = new Pos(); pos[key] = p; } p.Acct = ex.Account; p.Inst = ex.Instrument; p.LastExec = DateTime.UtcNow; Log("process " + key + " sgn=" + sgn + " qty=" + ex.Quantity + " px=" + ex.Price + " t=" + ex.Time + " netBefore=" + p.Net);
				double left = ex.Quantity;
				double price = ex.Price;
				while (left > 0)
				{
					if (Math.Abs(p.Net) < 1e-9)
					{
						p.Dir = sgn; p.Net = 0;
						p.EntQty = p.EntCost = p.ExQty = p.ExCost = 0;
						p.EntTime = ex.Time; p.FirstId = id;
					}
					if (sgn == p.Dir)
					{
						p.EntQty += left; p.EntCost += left * price; p.Net += sgn * left; left = 0;
					}
					else
					{
						double r = Math.Min(left, Math.Abs(p.Net));
						p.ExQty += r; p.ExCost += r * price; p.Net += sgn * r; left -= r;
						p.ExTime = ex.Time; p.LastId = id;
						if (Math.Abs(p.Net) < 1e-9) { Emit(an, ex.Instrument.FullName, p); p.Net = 0; }
					}
				} Log("   netAfter=" + p.Net); } catch (Exception px) { Log("Process ERROR (execution LOST): " + px); } } private static string Et(DateTime t)
		{
			TimeZoneInfo from = Globals.GeneralOptions.TimeZoneInfo;
			TimeZoneInfo et = TimeZoneInfo.FindSystemTimeZoneById("Eastern Standard Time");
			DateTime x = TimeZoneInfo.ConvertTime(DateTime.SpecifyKind(t, DateTimeKind.Unspecified), from, et);
			return x.ToString("yyyy-MM-dd HH:mm:ss", CultureInfo.InvariantCulture);
		}

		private static void Emit(string acct, string inst, Pos p)
		{
			if (p.EntQty <= 0 || p.ExQty <= 0) { Log("Emit skipped: entQty=" + p.EntQty + " exQty=" + p.ExQty); return; } if (p.ExTime < emitSince) { Log("Emit skipped (closed before today): " + inst + " " + p.ExTime); return; } // // old history: only used to rebuild positions
			string json = "{\"trade\":{\"id\":\"" + acct + "|" + inst + "|" + p.FirstId + "|" + p.LastId + "\","
				+ "\"acct\":\"" + acct + "\",\"inst\":\"" + inst + "\",\"dir\":\"" + (p.Dir > 0 ? "Long" : "Short") + "\","
				+ "\"qty\":" + p.EntQty.ToString(CultureInfo.InvariantCulture) + ","
				+ "\"ep\":" + (p.EntCost / p.EntQty).ToString("0.#####", CultureInfo.InvariantCulture) + ","
				+ "\"xp\":" + (p.ExCost / p.ExQty).ToString("0.#####", CultureInfo.InvariantCulture) + ","
				+ "\"et\":\"" + Et(p.EntTime) + "\",\"xt\":\"" + Et(p.ExTime) + "\"" + (p.Inst == null ? "" : EdgeOrderLog.ForTrade(acct, p.Inst.MasterInstrument.Name, p.EntTime, p.ExTime, p.Dir > 0)) + "}}";
			Log("EMIT " + json + " (stream " + (stream != null ? "up" : "DOWN, queued") + ")"); pending.Enqueue(json);
			while (pending.Count > 300) pending.Dequeue();
			System.Threading.ThreadPool.QueueUserWorkItem(delegate
				{
					lock (lk)
					{
						while (stream != null && pending.Count > 0)
						{
							if (!WriteLine(pending.Peek())) break;
							pending.Dequeue();
						}
					}
				});
		}
	}
}

#region NinjaScript generated code. Neither change nor remove.

namespace NinjaTrader.NinjaScript.Indicators
{
	public partial class Indicator : NinjaTrader.Gui.NinjaScript.IndicatorRenderBase
	{
		private EdgeFeed[] cacheEdgeFeed;
		public EdgeFeed EdgeFeed()
		{
			return EdgeFeed(Input);
		}

		public EdgeFeed EdgeFeed(ISeries<double> input)
		{
			if (cacheEdgeFeed != null)
				for (int idx = 0; idx < cacheEdgeFeed.Length; idx++)
					if (cacheEdgeFeed[idx] != null &&  cacheEdgeFeed[idx].EqualsInput(input))
						return cacheEdgeFeed[idx];
			return CacheIndicator<EdgeFeed>(new EdgeFeed(), input, ref cacheEdgeFeed);
		}
	}
}

namespace NinjaTrader.NinjaScript.MarketAnalyzerColumns
{
	public partial class MarketAnalyzerColumn : MarketAnalyzerColumnBase
	{
		public Indicators.EdgeFeed EdgeFeed()
		{
			return indicator.EdgeFeed(Input);
		}

		public Indicators.EdgeFeed EdgeFeed(ISeries<double> input )
		{
			return indicator.EdgeFeed(input);
		}
	}
}

namespace NinjaTrader.NinjaScript.Strategies
{
	public partial class Strategy : NinjaTrader.Gui.NinjaScript.StrategyRenderBase
	{
		public Indicators.EdgeFeed EdgeFeed()
		{
			return indicator.EdgeFeed(Input);
		}

		public Indicators.EdgeFeed EdgeFeed(ISeries<double> input )
		{
			return indicator.EdgeFeed(input);
		}
	}
}

#endregion
