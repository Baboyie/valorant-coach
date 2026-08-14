| Condition | Runs | Avg FPS | 1% low | 0.1% low | Frame-time stddev (ms) | Encode % | GPU 3D % |
|---|---|---|---|---|---|---|---|
| baseline | 3 | 217.8 | 136.5 | 29.9 | **1.885** | 9.5 | 17.7 |
| ours | 3 | 187.4 | 87.8 | 29.2 | **2.849** | 24.3 | 21 |

_1% low = FPS implied by the 99th-percentile frame time. Measured with Intel PresentMon 2.5.1 (console build, ETW only)._
