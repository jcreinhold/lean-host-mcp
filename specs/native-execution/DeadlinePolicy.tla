------------------------- MODULE DeadlinePolicy -------------------------
EXTENDS Naturals

\* Planned active-work policy, not the implemented queue-expiry/dispatch race.
\* Every transition samples one event at a bounded monotone time. Stopping is a
\* policy request, NOT observed process termination. No clock/OS liveness claim.
CONSTANTS HardLimit, SoftWindow, Bug
Events == {"activity", "heartbeat", "get", "quiet", "measurement_unavailable"}
Bugs == {"none", "extend-hard", "uncapped-soft", "heartbeat-renews",
         "revive-after-expiry", "missing-is-idle", "missing-bypasses-hard"}
ASSUME /\ HardLimit \in Nat \ {0}
       /\ SoftWindow \in 1..HardLimit
       /\ Bug \in Bugs

VARIABLES now, hard, soft, stopping, lastEvent, lastTime,
          priorSoft, priorStopping, priorExpired
vars == <<now, hard, soft, stopping, lastEvent, lastTime,
          priorSoft, priorStopping, priorExpired>>

Min(a,b) == IF a < b THEN a ELSE b
\* TLC compares set members eagerly enough that a string/numeric union is not
\* suitable here. Keep soft values uniformly numeric; NoSoft lies above every
\* bounded ordinary soft deadline, including the deliberate uncapped mutant.
NoSoft == HardLimit + SoftWindow + 2
Expired(t,h,s) == t >= h \/ (s # NoSoft /\ t >= s)

Init == /\ now = 0 /\ hard = HardLimit /\ soft = SoftWindow
        /\ stopping = FALSE /\ lastEvent = "init" /\ lastTime = 0
        /\ priorSoft = SoftWindow /\ priorStopping = FALSE
        /\ priorExpired = FALSE

Sample(e) ==
  LET due == stopping \/ Expired(now,hard,soft)
      renew == e = "activity" \/ (Bug = "heartbeat-renews" /\ e = "heartbeat")
      renewedSoft == IF Bug = "uncapped-soft" THEN now + SoftWindow
                     ELSE Min(now + SoftWindow, hard)
  IN
  /\ now <= HardLimit + 1
  /\ now' = now + 1
  /\ lastEvent' = e /\ lastTime' = now
  /\ priorSoft' = soft /\ priorStopping' = stopping
  /\ priorExpired' = Expired(now,hard,soft)
  /\ hard' = IF Bug = "extend-hard" /\ e = "activity" THEN hard + 1 ELSE hard
  /\ soft' = IF due THEN soft
              ELSE IF renew THEN renewedSoft
              ELSE IF e = "measurement_unavailable" THEN NoSoft
              ELSE soft
  /\ stopping' =
       IF Bug = "revive-after-expiry" /\ e = "activity" THEN FALSE
       ELSE IF Bug = "missing-bypasses-hard" /\
               e = "measurement_unavailable" /\ ~stopping THEN FALSE
       ELSE due \/ (Bug = "missing-is-idle" /\ e = "measurement_unavailable")

Next == \E e \in Events: Sample(e)
Spec == Init /\ [][Next]_vars

TypeOK == /\ now \in 0..(HardLimit + 2)
          /\ hard \in 0..(2 * HardLimit + 2)
          /\ soft \in {NoSoft} \cup 0..(HardLimit + SoftWindow + 1)
          /\ stopping \in BOOLEAN
          /\ lastEvent \in Events \cup {"init"}
          /\ lastTime \in 0..(HardLimit + 1)
          /\ priorSoft \in {NoSoft} \cup 0..(HardLimit + SoftWindow + 1)
          /\ priorStopping \in BOOLEAN /\ priorExpired \in BOOLEAN
FixedHardDeadline == hard = HardLimit
SoftWithinHard == soft = NoSoft \/ soft <= hard
NoRevival == ~priorStopping \/ stopping
ExpiryWins == ~priorExpired \/ stopping
HardSampleStops == lastEvent = "init" \/ lastTime < HardLimit \/ stopping
QuietEventsDoNotRenew ==
  lastEvent \notin {"heartbeat","get","quiet"} \/ priorStopping \/ priorExpired
  \/ soft = priorSoft
MissingSampleIsNotInactivity ==
  lastEvent # "measurement_unavailable" \/ priorStopping \/ priorExpired
  \/ (soft = NoSoft /\ ~stopping)
=============================================================================
