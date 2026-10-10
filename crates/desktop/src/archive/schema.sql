CREATE TABLE IF NOT EXISTS archive_partitions (
  viewer TEXT NOT NULL, community TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (viewer, community)
);
CREATE TABLE IF NOT EXISTS archive_subscriptions (
  viewer TEXT NOT NULL, community TEXT NOT NULL, name TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  days INTEGER NOT NULL, budget INTEGER NOT NULL,
  PRIMARY KEY (viewer, community, name)
);
CREATE TABLE IF NOT EXISTS archive_events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  viewer TEXT NOT NULL, community TEXT NOT NULL, subscription TEXT NOT NULL,
  id TEXT NOT NULL, agent TEXT NOT NULL, kind INTEGER NOT NULL,
  created INTEGER NOT NULL, received INTEGER NOT NULL, bytes INTEGER NOT NULL,
  envelope TEXT NOT NULL,
  UNIQUE (viewer, community, id)
);
CREATE INDEX IF NOT EXISTS archive_page ON archive_events(viewer, community, kind, seq DESC);
CREATE INDEX IF NOT EXISTS archive_agent_page ON archive_events(viewer, community, kind, agent, seq DESC);
CREATE INDEX IF NOT EXISTS archive_expiry ON archive_events(viewer, community, subscription, created);
CREATE TABLE IF NOT EXISTS archive_usage (
  viewer TEXT NOT NULL, community TEXT NOT NULL, subscription TEXT NOT NULL,
  agent TEXT NOT NULL, bytes INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(viewer,community,subscription,agent)
);
CREATE TRIGGER IF NOT EXISTS archive_insert_usage AFTER INSERT ON archive_events BEGIN
  INSERT INTO archive_usage VALUES(NEW.viewer,NEW.community,NEW.subscription,NEW.agent,NEW.bytes)
    ON CONFLICT(viewer,community,subscription,agent) DO UPDATE SET bytes=bytes+NEW.bytes;
  INSERT INTO archive_usage VALUES(NEW.viewer,NEW.community,NEW.subscription,'',NEW.bytes)
    ON CONFLICT(viewer,community,subscription,agent) DO UPDATE SET bytes=bytes+NEW.bytes;
END;
CREATE TRIGGER IF NOT EXISTS archive_delete_usage AFTER DELETE ON archive_events BEGIN
  UPDATE archive_usage SET bytes=bytes-OLD.bytes WHERE viewer=OLD.viewer AND community=OLD.community
    AND subscription=OLD.subscription AND agent IN ('',OLD.agent);
END;
