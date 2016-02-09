package lila.openingexplorer

import java.io.File

import fm.last.commons.kyoto.{ KyotoDb, WritableVisitor }
import fm.last.commons.kyoto.factory.{ KyotoDbBuilder, Mode, PageComparator }

import chess.{Hash, Situation, MoveOrDrop, PositionHash}

final class MasterDatabase extends MasterDatabasePacker {

  private val dbFile = new File("data/master.kct")
  dbFile.createNewFile

  private val db =
    new KyotoDbBuilder(dbFile)
      .modes(Mode.CREATE, Mode.READ_WRITE)
      .pageComparator(PageComparator.LEXICAL)
      .buckets(2000000L * 40)
      .buildAndOpen

  def probe(situation: Situation): SubEntry = probe(MasterDatabase.hash(situation))

  private def probe(h: PositionHash): SubEntry = {
    Option(db.get(h)) match {
      case Some(bytes) => unpack(bytes)
      case None        => SubEntry.empty
    }
  }

  def probeChildren(situation: Situation): List[(MoveOrDrop, SubEntry)] =
    Util.situationMovesOrDrops(situation).map { move =>
      move -> probe(move.fold(_.situationAfter, _.situationAfter))
    }.toList

  def merge(gameRef: GameRef, hashes: Set[PositionHash]) = {

    db.accept(hashes.toArray, new WritableVisitor {
      def record(key: PositionHash, value: Array[Byte]): Array[Byte] = {
        pack(unpack(value).withGameRef(gameRef))
      }

      def emptyRecord(key: PositionHash): Array[Byte] = pack(SubEntry.fromGameRef(gameRef))
    })
  }

  def close = {
    db.close()
  }

}

object MasterDatabase {

  val hash = new Hash(32)  // 128 bit Zobrist hasher

}
