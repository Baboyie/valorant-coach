| Condition | Runs | Avg FPS | 1% low | 0.1% low | Frame-time stddev (ms) | Encode % | GPU 3D % |
|---|---|---|---|---|---|---|---|
| baseline | 4 | 278.5 | 175.2 | 120 | **0.782** | 3.6 | 18.8 |
| ours | 4 | 266.7 | 169.5 | 132.9 | **0.67** | 15.3 | 20.8 |

_1% low = FPS implied by the 99th-percentile frame time. Measured with Intel PresentMon 2.5.1 (console build, ETW only)._
