"""Parser and rate-tracking tests for the machine-view host agent.

Everything here runs on fixture strings: no network binds, no /proc reads
outside the temporary fixtures, and no nvidia-smi execution.
"""

import os
import tempfile
import unittest

import machineview_agent as agent


class NvidiaSmiParsing(unittest.TestCase):
    def test_parses_well_formed_rows(self):
        output = (
            "0, NVIDIA RTX PRO 6000 Blackwell, 97, 81234, 97887, "
            "412.53, 71, 2617\n"
            "1, NVIDIA RTX PRO 6000 Blackwell, 3, 512, 97887, "
            "88.10, 45, 345\n"
        )
        gpus = agent.parse_nvidia_smi_csv(output)
        self.assertEqual(len(gpus), 2)
        self.assertEqual(gpus[0]["index"], 0)
        self.assertEqual(gpus[0]["util_pct"], 97.0)
        self.assertEqual(gpus[0]["mem_total_bytes"], 97887 * 1024 * 1024)
        self.assertAlmostEqual(gpus[0]["power_watts"], 412.53)
        self.assertEqual(gpus[1]["temp_c"], 45.0)

    def test_not_available_cells_become_none(self):
        output = "0, GPU, [N/A], 100, 200, [N/A], 50, 1000\n"
        gpus = agent.parse_nvidia_smi_csv(output)
        self.assertEqual(len(gpus), 1)
        self.assertIsNone(gpus[0]["util_pct"])
        self.assertIsNone(gpus[0]["power_watts"])

    def test_malformed_rows_are_skipped(self):
        self.assertEqual(agent.parse_nvidia_smi_csv("garbage\n1,2\n"), [])

    def test_extended_rows_carry_throttle_and_clock_fields(self):
        output = (
            "0, NVIDIA RTX PRO 6000 Blackwell, 97, 81234, 97887, 412.53, 71, "
            "2617, 64, 10251, 600.00, 45, P0, 78, Active, Not Active, "
            "Not Active, Not Active\n"
        )
        gpus = agent.parse_nvidia_smi_csv(output)
        self.assertEqual(len(gpus), 1)
        gpu = gpus[0]
        self.assertEqual(gpu["mem_util_pct"], 64.0)
        self.assertEqual(gpu["mem_clock_mhz"], 10251.0)
        self.assertEqual(gpu["power_limit_watts"], 600.0)
        self.assertEqual(gpu["fan_pct"], 45.0)
        self.assertEqual(gpu["pstate"], 0.0)
        self.assertEqual(gpu["temp_mem_c"], 78.0)
        self.assertEqual(gpu["throttle_sw_power"], 1.0)
        self.assertEqual(gpu["throttle_sw_thermal"], 0.0)
        self.assertEqual(gpu["throttle_hw"], 0.0)


class ProcParsing(unittest.TestCase):
    def test_cpu_line_busy_and_total(self):
        text = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4 5 6 7 8 9 0\n"
        busy, total, iowait = agent.parse_proc_stat_cpu(text)
        self.assertEqual(total, 1000)
        self.assertEqual(busy, 150)
        self.assertEqual(iowait, 50)

    def test_cpu_line_missing_returns_none(self):
        self.assertIsNone(agent.parse_proc_stat_cpu("intr 1 2 3\n"))

    def test_meminfo_fields(self):
        text = (
            "MemTotal:       1000 kB\n"
            "MemAvailable:    400 kB\n"
            "Buffers:          50 kB\n"
            "Cached:          150 kB\n"
            "SwapTotal:       500 kB\n"
            "SwapFree:        300 kB\n"
        )
        memory = agent.parse_meminfo(text)
        self.assertEqual(memory["mem_total_bytes"], 1000 * 1024)
        self.assertEqual(memory["mem_used_bytes"], 600 * 1024)
        self.assertEqual(memory["mem_cached_bytes"], 200 * 1024)
        self.assertEqual(memory["swap_used_bytes"], 200 * 1024)
        self.assertIsNone(memory["dirty_bytes"])
        self.assertIsNone(memory["writeback_bytes"])

    def test_net_dev_skips_virtual_interfaces(self):
        text = (
            "Inter-|   Receive                                                "
            "|  Transmit\n"
            " face |bytes    packets errs drop fifo frame compressed multicast"
            "|bytes    packets errs drop fifo colls carrier compressed\n"
            "    lo: 999 1 0 0 0 0 0 0 999 1 0 0 0 0 0 0\n"
            "  eth0: 1000 1 0 0 0 0 0 0 2000 1 0 0 0 0 0 0\n"
            "docker0: 500 1 0 0 0 0 0 0 500 1 0 0 0 0 0 0\n"
            "  eno1: 100 1 0 0 0 0 0 0 200 1 0 0 0 0 0 0\n"
        )
        rx, tx = agent.parse_net_dev(text)
        self.assertEqual(rx, 1100)
        self.assertEqual(tx, 2200)

    def test_diskstats_whole_disks_only(self):
        text = (
            "   8       0 sda 100 0 2000 0 50 0 1000 0 0 0 0 0 0 0\n"
            "   8       1 sda1 90 0 1800 0 40 0 900 0 0 0 0 0 0 0\n"
            " 259       0 nvme0n1 10 0 400 0 5 0 200 0 0 0 0 0 0 0\n"
            " 259       1 nvme0n1p1 9 0 390 0 4 0 190 0 0 0 0 0 0 0\n"
        )
        disk = agent.parse_diskstats(text)
        self.assertEqual(disk["read_bytes"], (2000 + 400) * 512)
        self.assertEqual(disk["write_bytes"], (1000 + 200) * 512)
        self.assertEqual(disk["reads"], 110)
        self.assertEqual(disk["writes"], 55)
        self.assertEqual(disk["inflight"], 0)
        self.assertEqual(set(disk["io_ticks"]), {"sda", "nvme0n1"})

    def test_pressure_some_avg10(self):
        text = (
            "some avg10=2.50 avg60=1.00 avg300=0.25 total=12345\n"
            "full avg10=0.10 avg60=0.00 avg300=0.00 total=9\n"
        )
        self.assertEqual(agent.parse_pressure(text), 2.5)
        self.assertIsNone(agent.parse_pressure("full avg10=9.00 total=1\n"))
        self.assertIsNone(agent.parse_pressure(None))

    def test_loadavg(self):
        self.assertEqual(agent.parse_loadavg("1.25 2.0 3.0 1/234 999\n"), 1.25)
        self.assertIsNone(agent.parse_loadavg("nonsense"))


class CollectorRates(unittest.TestCase):
    def _write(self, root, relative, content):
        path = os.path.join(root, relative)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(content)

    def test_first_sample_has_null_rates_then_values(self):
        with tempfile.TemporaryDirectory() as root:
            proc = os.path.join(root, "proc")
            self._write(proc, "stat", "cpu 100 0 100 800 0 0 0 0 0 0\n")
            self._write(proc, "meminfo", "MemTotal: 1000 kB\nMemAvailable: 500 kB\n")
            self._write(proc, "loadavg", "0.5 0.4 0.3 1/2 3\n")
            self._write(
                proc,
                "net/dev",
                "  eth0: 1000 1 0 0 0 0 0 0 2000 1 0 0 0 0 0 0\n",
            )
            self._write(
                proc,
                "diskstats",
                "   8 0 sda 1 0 100 0 1 0 100 0 0 0 0 0 0 0\n",
            )
            collector = agent.Collector(
                mounts=[root],
                proc_root=proc,
                rapl_root=os.path.join(root, "no-rapl"),
            )
            collector.nvidia_smi = None
            first = collector.sample()
            self.assertEqual(first["version"], 1)
            self.assertIsNone(first["host"]["cpu_pct"])
            self.assertIsNone(first["host"]["net_rx_bps"])
            self.assertEqual(first["host"]["mem_total_bytes"], 1000 * 1024)
            self.assertEqual(first["gpus"], [])
            self.assertEqual(len(first["host"]["disks"]), 1)
            self.assertEqual(first["host"]["disks"][0]["mount"], root)
            self.assertGreater(first["host"]["disks"][0]["inodes_total"], 0)
            self.assertIn("inodes_used", first["host"]["disks"][0])
            self.assertIsNone(first["host"]["iowait_pct"])
            self.assertIsNone(first["host"]["io_pressure_pct"])

            self._write(proc, "stat", "cpu 200 0 200 1400 0 0 0 0 0 0\n")
            self._write(
                proc,
                "net/dev",
                "  eth0: 3000 1 0 0 0 0 0 0 2500 1 0 0 0 0 0 0\n",
            )
            second = collector.sample()
            self.assertAlmostEqual(second["host"]["cpu_pct"], 25.0)
            self.assertIsNotNone(second["host"]["net_rx_bps"])
            self.assertGreater(second["host"]["net_rx_bps"], 0)

    def test_counter_reset_yields_null_rate(self):
        with tempfile.TemporaryDirectory() as root:
            proc = os.path.join(root, "proc")
            self._write(
                proc,
                "net/dev",
                "  eth0: 5000 1 0 0 0 0 0 0 5000 1 0 0 0 0 0 0\n",
            )
            collector = agent.Collector(
                mounts=[], proc_root=proc, rapl_root=os.path.join(root, "none")
            )
            collector.nvidia_smi = None
            collector.sample()
            self._write(
                proc,
                "net/dev",
                "  eth0: 10 1 0 0 0 0 0 0 10 1 0 0 0 0 0 0\n",
            )
            reset = collector.sample()
            self.assertIsNone(reset["host"]["net_rx_bps"])

    def test_rapl_package_domains_only(self):
        with tempfile.TemporaryDirectory() as root:
            rapl = os.path.join(root, "powercap")
            for name, energy in (
                ("intel-rapl:0", "1000000"),
                ("intel-rapl:0:0", "999999999"),
                ("intel-rapl:1", "2000000"),
            ):
                os.makedirs(os.path.join(rapl, name), exist_ok=True)
                with open(
                    os.path.join(rapl, name, "energy_uj"), "w", encoding="utf-8"
                ) as handle:
                    handle.write(energy)
            collector = agent.Collector(
                mounts=[], proc_root=os.path.join(root, "proc"), rapl_root=rapl
            )
            collector.nvidia_smi = None
            first = collector.sample()
            self.assertIsNone(first["host"]["cpu_watts"])
            for name, energy in (
                ("intel-rapl:0", "2000000"),
                ("intel-rapl:1", "4000000"),
            ):
                with open(
                    os.path.join(rapl, name, "energy_uj"), "w", encoding="utf-8"
                ) as handle:
                    handle.write(energy)
            second = collector.sample()
            self.assertIsNotNone(second["host"]["cpu_watts"])
            self.assertGreater(second["host"]["cpu_watts"], 0)

    def test_iowait_pressure_and_disk_busy(self):
        with tempfile.TemporaryDirectory() as root:
            proc = os.path.join(root, "proc")
            self._write(proc, "stat", "cpu 100 0 100 700 100 0 0 0 0 0\n")
            self._write(
                proc,
                "diskstats",
                "   8 0 sda 10 0 100 0 10 0 100 0 2 1000 0 0\n",
            )
            self._write(
                proc,
                "pressure/io",
                "some avg10=12.5 avg60=4.0 avg300=1.0 total=9\n"
                "full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
            )
            self._write(
                proc,
                "meminfo",
                "MemTotal: 1000 kB\nMemAvailable: 400 kB\n"
                "Dirty: 80 kB\nWriteback: 20 kB\n",
            )
            collector = agent.Collector(
                mounts=[],
                proc_root=proc,
                rapl_root=os.path.join(root, "no-rapl"),
            )
            collector.nvidia_smi = None
            collector.sample()
            self._write(proc, "stat", "cpu 150 0 150 1100 200 0 0 0 0 0\n")
            self._write(
                proc,
                "diskstats",
                "   8 0 sda 20 0 300 0 40 0 500 0 3 1500 0 0\n",
            )
            second = collector.sample()
            host = second["host"]
            # total jiffies 1000 → 1600, iowait 100 → 200, busy 200 → 300
            self.assertAlmostEqual(host["cpu_pct"], 100.0 * 100 / 600)
            self.assertAlmostEqual(host["iowait_pct"], 100.0 * 100 / 600)
            self.assertEqual(host["io_pressure_pct"], 12.5)
            self.assertEqual(host["dirty_bytes"], 80 * 1024)
            self.assertEqual(host["writeback_bytes"], 20 * 1024)
            self.assertEqual(host["disk_inflight"], 3.0)
            self.assertGreater(host["disk_read_iops"], 0)
            self.assertGreater(host["disk_write_iops"], 0)
            self.assertIsNotNone(host["disk_util_pct"])
            self.assertGreater(host["disk_util_pct"], 0)


if __name__ == "__main__":
    unittest.main()
