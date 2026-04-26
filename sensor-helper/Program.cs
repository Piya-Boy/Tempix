using System.Globalization;
using LibreHardwareMonitor.Hardware;

var computer = new Computer
{
    IsCpuEnabled = true,
};

computer.Open();
var visitor = new UpdateVisitor();

if (args.Contains("--dump", StringComparer.OrdinalIgnoreCase))
{
    computer.Accept(visitor);
    var writer = CreateDumpWriter(args);
    foreach (var item in WalkHardware(computer.Hardware))
    {
        writer.WriteLine($"Hardware: {item.HardwareType} | {item.Name} | {item.Identifier}");
        foreach (var sensor in item.Sensors)
        {
            writer.WriteLine($"  {sensor.SensorType} | {sensor.Name} | {sensor.Value?.ToString(CultureInfo.InvariantCulture) ?? "-"} | {sensor.Identifier}");
        }
    }
    writer.Dispose();
    return;
}

var once = args.Contains("--once", StringComparer.OrdinalIgnoreCase);
do
{
    try
    {
        computer.Accept(visitor);
        var temp = FindCpuTemperature(computer.Hardware);
        Console.WriteLine(temp.HasValue
            ? temp.Value.ToString("F1", CultureInfo.InvariantCulture)
            : "-");
        Console.Out.Flush();
    }
    catch
    {
        Console.WriteLine("-");
        Console.Out.Flush();
    }

    if (!once)
    {
        Thread.Sleep(2000);
    }
} while (!once);

static float? FindCpuTemperature(IEnumerable<IHardware> hardware)
{
    var bestScore = 0;
    float? bestTemp = null;

    foreach (var item in WalkHardware(hardware))
    {
        foreach (var sensor in item.Sensors)
        {
            if (sensor.SensorType != SensorType.Temperature || !sensor.Value.HasValue)
            {
                continue;
            }

            var temp = sensor.Value.Value;
            if (temp is < 1.0f or > 125.0f)
            {
                continue;
            }

            var score = ScoreCpuSensor(item, sensor);
            if (score <= 0)
            {
                continue;
            }

            if (score > bestScore || (score == bestScore && (!bestTemp.HasValue || temp > bestTemp.Value)))
            {
                bestScore = score;
                bestTemp = temp;
            }
        }
    }

    return bestTemp;
}

static TextWriter CreateDumpWriter(string[] args)
{
    var index = Array.FindIndex(args, arg => arg.Equals("--dump-file", StringComparison.OrdinalIgnoreCase));
    if (index >= 0 && index + 1 < args.Length)
    {
        return new StreamWriter(args[index + 1], false);
    }

    return Console.Out;
}

static IEnumerable<IHardware> WalkHardware(IEnumerable<IHardware> hardware)
{
    foreach (var item in hardware)
    {
        yield return item;

        foreach (var child in WalkHardware(item.SubHardware))
        {
            yield return child;
        }
    }
}

static int ScoreCpuSensor(IHardware hardware, ISensor sensor)
{
    var hardwareText = $"{hardware.HardwareType} {hardware.Name} {hardware.Identifier}".ToLowerInvariant();
    var sensorText = $"{sensor.Name} {sensor.Identifier}".ToLowerInvariant();
    var text = hardwareText + " " + sensorText;

    if (text.Contains("gpu") || text.Contains("nvidia") || text.Contains("radeon"))
    {
        return 0;
    }

    var score = hardware.HardwareType == HardwareType.Cpu ? 60 : 0;
    if (sensorText.Contains("package")) score += 70;
    if (sensorText.Contains("tctl") || sensorText.Contains("tdie")) score += 65;
    if (sensorText.Contains("ccd")) score += 60;
    if (sensorText.Contains("cpu")) score += 55;
    if (sensorText.Contains("core")) score += 45;
    if (hardwareText.Contains("cpu")) score += 40;
    if (hardwareText.Contains("intel") || hardwareText.Contains("amd") || hardwareText.Contains("ryzen")) score += 30;

    return score;
}

sealed class UpdateVisitor : IVisitor
{
    public void VisitComputer(IComputer computer) => computer.Traverse(this);
    public void VisitHardware(IHardware hardware)
    {
        hardware.Update();
        foreach (var child in hardware.SubHardware)
        {
            child.Accept(this);
        }
    }
    public void VisitSensor(ISensor sensor) { }
    public void VisitParameter(IParameter parameter) { }
}
