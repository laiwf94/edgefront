Medians of 3 repetitions; p99 ranges in brackets.

| Scenario | Conns | Offered | Throughput steer / edgefront | p50 steer / edgefront | p99 steer | p99 edgefront | Peak RSS steer / edgefront | CPU s steer / edgefront |
|---|---|---|---|---|---|---|---|---|
| steer-port-http1-1 | 1 | 500/s | 500 / 500 | 0.145 / 0.143 ms | 0.352 ms (0.322 to 2.550) | 2.984 ms (0.334 to 6.160) | 11.1 MiB / 0.8 MiB | 2.3 / 1.8 |
| steer-port-http1-64 | 64 | 20,000/s | 20,000 / 20,000 | 0.155 / 0.144 ms | 27.869 ms (3.326 to 2086.666) | 2.275 ms (0.486 to 5.636) | 15.3 MiB / 10.3 MiB | 41.3 / 44.6 |
| steer-port-http1-10000 | 10,000 | 20,000/s | 20,000 / 20,000 | 0.128 / 0.131 ms | 1.525 ms (0.318 to 7.975) | 1.211 ms (0.219 to 6.668) | 337.5 MiB / 220.9 MiB | 39.1 / 46.4 |
| steer-port-tls-1 | 1 | 500/s | 500 / 500 | 0.190 / 0.163 ms | 97.780 ms (1.609 to 296.485) | 13.885 ms (4.764 to 912.785) | 9.7 MiB / 0.9 MiB | 2.9 / 2.1 |
| steer-port-tls-64 | 64 | 20,000/s | 20,000 / 20,000 | 0.154 / 0.126 ms | 11.280 ms (7.786 to 476.840) | 0.438 ms (0.275 to 2.525) | 16.7 MiB / 9.4 MiB | 41.6 / 43.4 |
| steer-port-tls-10000 | 10,000 | 20,000/s | 20,000 / 20,000 | 0.153 / 0.149 ms | 6.488 ms (1.266 to 12.960) | 3.617 ms (0.401 to 17.859) | 495.3 MiB / 309.3 MiB | 43.4 / 48.4 |

| Ramp | steer: highest step held | edgefront: highest step held |
|---|---|---|
| steer-port-http1-64-ramp | 20,000/s (runs: 20,000, 60,000, 0) | 20,000/s (runs: 20,000, 40,000, 20,000) |
| steer-port-tls-64-ramp | 60,000/s (runs: 60,000, 20,000, 60,000) | 60,000/s (runs: 60,000, 60,000, 60,000) |

| Target | Binary | Image |
|---|---|---|
| edgefront-proxy | 7.8 MiB | 13.6 MiB |
| edgefront-proxy-tls | 7.8 MiB | 13.6 MiB |
| steer | 7.3 MiB | 13.5 MiB |
| steer-tls | 7.3 MiB | 13.5 MiB |
