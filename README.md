# Poly-Latency Arbitrage

Think of this as a **short-horizon event-driven relative-value strategy**, not a “scraper bot.” The hypothesis is that prediction-market quotes impound new public information with a delay; your system measures whether that delay leaves a risk-adjusted, executable profit after transaction costs and adverse selection. A proper quant workflow separates signal generation, execution assumptions, risk limits, and out-of-sample validation rather than treating a positive-looking backtest as proof. [info.arkm](https://info.arkm.com/research/trading-systems-automations-algorithms-copy-trading-and-crypto-trading-bots)

## The economic hypothesis

For a market $m$ tied to a public release $r$, define the market’s implied probability $P_m(t)$ and the “true” post-release probability $P^*_r$.

Your null hypothesis is:

$$H_0:\quad P_m(t)=P^*_r \quad \text{for all }t\geq t_{\text{publication}}$$

Your alternative hypothesis is:

$$H_A:\quad P_m(t)<P^*_r
\quad \text{for some interval } t\in[t_{\text{publication}},t_{\text{repriced}})$$

You are not claiming you can predict the future. You are claiming that, conditional on a clean release, the market’s conditional probability is temporarily inconsistent with the newly public state of the world.

## Signal definition

A signal is not “the API changed.” It is a validated state transition:

$$S_i =
\mathbf{1}\{
\text{source authoritative}
\wedge
\text{schema valid}
\wedge
\text{value maps deterministically}
\wedge
\text{market open}
\wedge
\text{book healthy}
\}$$

Every observation must resolve to one of:

$$S_i\in\{\text{valid signal},\text{no signal},\text{ambiguous},\text{parse failure},\text{stale cache}\}$$

This distinction matters because your denominator is not “number of times the bot fired.” It is the number of **eligible release events**. If you only log successful signals, you implicitly condition on your own system working—which biases the edge upward.

## The timing model

Polling creates an interval, not an instant. If the prior poll returned old state at $t^-_i$, and the next poll first returns new state at $t^+_i$, the true publication time lies in:

$$t_{\text{pub},i}\in[t^-_i,t^+_i]$$

So the conservative estimate of your informational lead is:

$$\widehat{\Delta}_i^{\text{cons}}
=
t_{\text{react},i}
-
t^-_i$$

not:

$$t_{\text{react},i}-t^+_i$$

The first version assumes the source changed immediately after your previous request. That is deliberately pessimistic: if the edge survives it, the result is more credible.

Your execution path is:

$$T_{\text{you}}
=
T_{\text{detect}}
+
T_{\text{parse}}
+
T_{\text{decide}}
+
T_{\text{transit}}
+
T_{\text{match}}$$

The opportunity exists only if:

$$T_{\text{react},i}>T_{\text{you},i}$$

But that is necessary, not sufficient. The quote must also still be there when your order would arrive.

## From price change to PnL

Do not measure “the ask moved from 65 cents to 99 cents.” Measure the **executable VWAP** available after a realistic delay.

For a buy of the winning outcome, let the ask ladder at time $t_i+\tau$ be:

$$\mathcal{A}_i(\tau)=\{(q_{k},a_{k})\}_{k=1}^{K}$$

where $q_k$ is price and $a_k$ is available size. For target notional $N$, the simulated fill is:

$$Q_i(\tau)=\sum_{k=1}^{K} q_k a_k
\quad\text{s.t.}\quad
\sum_{k=1}^{K} a_k\geq \frac{N}{\bar q_i(\tau)}$$

The VWAP is:

$$\bar q_i(\tau)
=
\frac{\sum_k q_k a_k}{\sum_k a_k}$$

For a binary contract that resolves to $1$, expected gross profit per share is:

$$1-\bar q_i(\tau)$$

After costs $\phi$, the conservative edge is:

$$e_i(\tau)
=
1-\bar q_i(\tau)-\phi$$

And the executable edge condition is:

$$\boxed{
e_i(\tau)>0
\quad\land\quad
\text{Depth}_i(\tau)\geq D_{\min}
\quad\land\quad
\text{BookHealth}_i(\tau)=\text{Healthy}
}$$

If $e_i(0)>0$ but $e_i(100\text{ ms})\leq0$, classify it as `PHANTOM_EDGE`. It may have looked tradeable in a log, but it was not executable at your realistic latency.

## The full event PnL

For each eligible event $i$, define:

$$\Pi_i
=
\mathbf{1}\{\text{filled}\}
\left[
Q_i
-
N_i\bar q_i(\tau_i)
-
\phi_i
-
L^{\text{adv}}_i
\right]
-
C_i^{\text{ops}}$$

where:

- $Q_i$ = settlement value of filled shares.
- $N_i$ = shares acquired.
- $\bar q_i(\tau_i)$ = realized or conservatively simulated VWAP.
- $\phi_i$ = explicit and implicit transaction costs.
- $L^{\text{adv}}_i$ = expected loss from trading against better-informed flow.
- $C_i^{\text{ops}}$ = infrastructure, data, and failure costs.

The strategy’s population-level expectation is:

$$\boxed{
\mathbb{E}[\Pi]
=
\frac{1}{N}
\sum_{i=1}^{N}\Pi_i
}$$

But do not judge it by mean PnL alone. With rare, fat-tailed events, estimate the distribution:

$$\widehat{\text{VaR}}_\alpha
=
\inf\{x:\Pr(\Pi\leq x)\geq\alpha\}$$

and expected shortfall:

$$\widehat{\text{ES}}_\alpha
=
\mathbb{E}[\Pi\mid\Pi\leq\widehat{\text{VaR}}_\alpha]$$

A strategy with a positive mean but catastrophic left tail is not automatically viable.

## Statistical test

Your unit of observation is the **independent release event**, not the individual WebSocket message.

Let $X_i=1$ if event $i$ produced a conservative executable edge, and $0$ otherwise. Under a null hypothesis of no edge:

$$H_0:p\leq p_0$$

where $p_0$ is the minimum hit rate required for the strategy to clear costs. Test:

$$\widehat p=\frac{1}{N}\sum_{i=1}^{N}X_i$$

with a one-sided binomial test. More importantly, compute a confidence interval:

$$\widehat p\pm z_{\alpha/2}
\sqrt{\frac{\widehat p(1-\widehat p)}{N}}$$

If the lower confidence bound is below your cost-adjusted breakeven hit rate, you do not yet have evidence of an edge.

Also run the analysis out-of-sample: fit nothing on the events used for the final decision. A strategy tuned after seeing the data is vulnerable to overfitting, lookahead bias, and survivorship bias. [info.arkm](https://info.arkm.com/research/trading-systems-automations-algorithms-copy-trading-and-crypto-trading-bots)

## Risk constraints

Before any order, the system must satisfy:

$$\boxed{
\begin{aligned}
&|x_m|\leq X_m \\
&\sum_m |x_m|\leq X_{\text{total}} \\
&\sum_{i\in\text{day}}\Pi_i^- \leq L_{\text{day}} \\
&\text{BookHealth}=\text{Healthy} \\
&\text{SignalConfidence}\geq c_{\min} \\
&\text{KillSwitch}=\text{off}
\end{aligned}
}$$

The important risk is not that you are wrong about the release. It is that you are right about the release but wrong about the **market’s resolution rule**, liquidity, queue position, or counterparty behavior.

## What the v1 harness measures

| Layer | Quant question | Output |
|---|---|---|
| Origin | When did the information become available, within what uncertainty? | Publication interval, detection lag |
| Parser | Did the source deterministically imply an outcome? | Signal/no-signal/ambiguous |
| Book | Was the market state trustworthy? | Health, sequence gaps, staleness |
| Reaction | Did the book materially reprice, and when? | Reaction latency distribution |
| Liquidity | Was there enough depth at a favorable price? | VWAP, depth, persistence |
| Execution | Would the trade have survived realistic delay? | Forward-walk edge decay |
| Economics | Was expected profit positive after costs? | Net edge, fee sensitivity |
| Statistics | Is the result repeatable across independent events? | Hit rate, confidence interval, out-of-sample result |

## The proper conclusion

Do not say:

> “The origin updated 1.8 seconds before Polymarket, so there is edge.”

Say:

> “Across $N$ predeclared, independent release events, $k\%$ produced a conservative executable edge at a $100$ ms execution delay, with median available depth $D$, median post-delay VWAP $q$, and a $95\%$ confidence interval for the executable-edge rate of $[a,b]$.”

That is the difference between a story and a strategy. A quant system is judged by whether its edge survives realistic costs, execution assumptions, risk limits, and out-of-sample data—not by whether a single latency gap looks impressive. [quantt.co](https://www.quantt.co.uk/resources/quant-trading-complete-guide) It would be called a **low-latency event-driven relative-value strategy with an information-diffusion alpha signal**—more specifically, a **public-information latency arbitrage model** for prediction markets.

## Proper name

```text
Event-Driven Information-Diffusion Relative-Value Strategy
for Binary Prediction Markets
```

Shorter, desk-style name:

```text
Public-Release Latency Arbitrage (PRLA)
```

Or, if you want it to sound like an internal quant-desk model:

```text
Time-to-Price (T2P) Alpha Model
```

## What it actually is

It is **not** statistical arbitrage in the classic pairs-trading sense, because you are not modeling a stationary spread between two assets. It is closer to an **event-study alpha model**:

$$r_{m,t}
=
\alpha
+
\beta \mathbf{1}\{\text{release}\}
+
\gamma \mathbf{1}\{\text{release}\}\cdot f(\text{latency})
+
\varepsilon_t$$

Your alpha is the interaction term: a release event creates a temporary mispricing, and your ability to capture it depends on detecting the release before the market reprices.

More precisely, it is a **conditional event-study model with an execution-constrained alpha filter**.

## The formal classification

| Component | Quant label |
|---|---|
| Strategy family | Event-driven relative value |
| Alpha source | Information diffusion / public-data latency |
| Signal type | Deterministic release-triggered state update |
| Market | Binary prediction contracts / event contracts |
| Execution style | Liquidity-taking, latency-sensitive |
| Risk model | Event, liquidity, adverse-selection, and operational risk |
| Validation | Event-level out-of-sample hypothesis test |
| Current stage | Alpha research / falsification harness |

So the most accurate one-line title is:

```text
Low-Latency Event-Driven Information-Diffusion Alpha Model
for Prediction-Market Liquidity Taking
```

## How a desk would describe it

A quant researcher would say:

> “We run an event-driven relative-value strategy on binary prediction markets. We identify recurring public-information releases with deterministic resolution rules, estimate the market’s price-discovery lag relative to the primary source, and take liquidity only when the post-signal executable VWAP remains profitable after fees, execution delay, and adverse-selection costs.”

That is the proper version of “Polymarket Time Machine.”

## The model’s formal objective

$$\boxed{
\max_{\tau,x}
\quad
\mathbb{E}\!\left[
\left(v-q_\tau-\phi\right)x_\tau
-
L_{\text{adv}}(x_\tau)
-
C_{\text{ops}}
\right]
}$$

subject to:

$$\begin{aligned}
&\Pr(\text{book healthy})\geq1-\epsilon \\
&x_\tau\leq X_{\max} \\
&\text{VaR}_\alpha(\Pi)\geq -L_{\max} \\
&\text{quote persistence}(\tau)\geq P_{\min} \\
&\text{resolution certainty}=1
\end{aligned}$$

This makes it clear that the model is not “buy the winning side.” It is **selectively provide-less, take-more liquidity only when the event, book state, depth, and execution path jointly satisfy a predeclared profitability constraint**.

## What makes it quant rather than a bot

A bot says:

> “API changed; buy YES.”

Your model says:

> “Conditional on a valid, unambiguous release; a healthy, sequence-consistent book; sufficient post-delay depth; a predeclared repricing threshold; and positive expected value after fees and adverse selection, submit an immediate-or-cancel order with bounded size.”

That conditional structure is what makes it a quant model.

## Best internal codename

If you want something that sounds credible but still has personality:

```text
Project CHRONOS
Causal High-frequency Resolution
Observation & Market-order System
```

Or, more restrained:

```text
Project LATENCY
Latency-Aware Trade Execution &
Notification Yield Analysis
```

My pick for the actual internal name:

```text
T2P: Time-to-Price Alpha Engine
```

Because the entire thesis reduces to one measurable object:

$$\boxed{
\text{T2P}_i
=
t_{\text{market repriced},i}
-
t_{\text{origin available},i}
}$$

Your strategy is profitable only if:

$$\boxed{
\text{T2P}_i
>
T_{\text{you},i}
+
\text{cost-adjusted margin}
}$$

repeatedly, across independent events, with real executable liquidity. Quant trading is fundamentally about turning data and models into systematic, risk-aware trading decisions—not merely automating a manual trade. [ig](https://www.ig.com/en/trading-strategies/a-traders-guide-to-quantitative-trading-200420)
